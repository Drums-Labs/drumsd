"""Command construction and process identity for the shared-OS spike. Pure functions + a Host
abstraction so everything except the actual syscalls can be tested with fakes.

CRIU flags follow criu(8) and were NOT executed against a real CRIU in this work (CRIU is absent
from the development sandbox). Whether they are sufficient for a namespaced X tree is exactly what
the diagnostic gates measure.
"""

from __future__ import annotations

import os
import shutil
import signal
import sys
import time

TOOLS = ("criu", "unshare", "nsenter", "setsid", "Xvfb", "xterm")
PKG_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STATE_SOCK = "/tmp/state.sock"


def launch_argv(marker: str, display: str, log: str = "/dev/null") -> tuple:
    """Detached launch. stdio is redirected inside the wrapper: otherwise the namespace tree inherits
    the caller's capture pipes and the launching command never returns."""
    return ("setsid", "-f", "sh", "-c", 'exec "$@" </dev/null >>"$0" 2>&1', log,
            "unshare", "--pid", "--fork", "--mount-proc", "--mount", "--net", "--ipc", "--uts",
            "--propagation", "private", sys.executable, "-m", "sharedos_spike.tree",
            "--marker", marker, "--display", display)


def dump_argv(pid: int, images: str, log: str) -> tuple:
    return ("criu", "dump", "-t", str(pid), "-D", images, "--leave-running", "--shell-job", "-v4", "-o", log)


def restore_argv(images: str, pidfile: str, log: str) -> tuple:
    return ("criu", "restore", "-D", images, "--restore-detached", "--shell-job", "--pidfile", pidfile,
            "-v4", "-o", log)


def _enter(pid: int) -> tuple:
    return ("nsenter", "--target", str(pid), "--mount", "--pid")


def state_probe_argv(pid: int, setting: str | None = None) -> tuple:
    tail = ["--set", setting] if setting else []
    return (*_enter(pid), sys.executable, "-m", "sharedos_spike.stateapp", "query", STATE_SOCK, *tail)


def x_probe_argv(pid: int, display: str, png: bool = False) -> tuple:
    return (*_enter(pid), sys.executable, "-m", "sharedos_spike.xprobe", display, *(["--png-b64"] if png else []))


def x_title_argv(pid: int, display: str) -> tuple:
    return (*_enter(pid), sys.executable, "-m", "sharedos_spike.xtitle", display)


def procs_argv(pid: int) -> tuple:
    return (*_enter(pid), sys.executable, "-m", "sharedos_spike.procprobe")


def env_with_pkg_root() -> dict:
    return {"PYTHONPATH": PKG_ROOT + os.pathsep + os.environ.get("PYTHONPATH", "")}


def tree_argv_matches(argv, marker: str, module: str = "sharedos_spike.tree") -> bool:
    """Exact: `-m <module> --marker <marker>` appear contiguously in argv."""
    want = ["-m", module, "--marker", marker]
    return bool(argv) and any(argv[i:i + 4] == want for i in range(len(argv) - 3))


class Host:
    """The real host. Tests substitute FakeHost with the same methods."""

    def __init__(self, proc: str = "/proc", module: str = "sharedos_spike.tree", kill_wait_s: float = 60.0):
        self.proc = proc
        self.module = module   # the python module whose `-m <module> --marker M` argv identifies this host's processes
        # How long a SIGKILLed process may take to finish exiting. A pid-namespace init exits only after every task in its
        # namespace has exited (zap_pid_ns_processes); on a saturated host tearing down a GNOME tree takes seconds, not 0.5 s.
        self.kill_wait_s = kill_wait_s

    def which(self, tool: str):
        return shutil.which(tool)

    def euid(self) -> int:
        return os.geteuid()

    def has_pidfd(self) -> bool:
        return hasattr(os, "pidfd_open") and hasattr(signal, "pidfd_send_signal")

    def _argv(self, pid: str):
        try:
            with open(f"{self.proc}/{pid}/cmdline", "rb") as f:
                return [x.decode(errors="replace") for x in f.read().split(b"\0") if x]
        except OSError:
            return None

    def _nspid(self, pid: str):
        try:
            with open(f"{self.proc}/{pid}/status") as f:
                for line in f:
                    if line.startswith("NSpid:"):
                        return line.split()[1:]
        except OSError:
            pass
        return None

    def find_init(self, marker: str):
        """Host pid of the slot's namespace init: exact argv match and pid 1 in its own pid ns."""
        hits = []
        for name in os.listdir(self.proc):
            if name.isdigit() and tree_argv_matches(self._argv(name), marker, self.module):
                ns = self._nspid(name)
                if ns and len(ns) >= 2 and ns[-1] == "1":
                    hits.append(int(name))
        return hits[0] if len(hits) == 1 else None

    def listdir_nonempty(self, path: str) -> bool:
        try:
            return bool(os.listdir(path))
        except OSError:
            return False

    def path_exists(self, path: str) -> bool:
        return os.path.exists(path)

    def read_pid(self, pidfile: str):
        try:
            with open(pidfile) as f:
                return int(f.read().strip())
        except (OSError, ValueError):
            return None

    def find_all_inits(self, marker: str) -> list:
        """Every process that is an exact-marker tree init (pid 1 of its own pid namespace)."""
        out = []
        for name in sorted((n for n in os.listdir(self.proc) if n.isdigit()), key=int):
            if tree_argv_matches(self._argv(name), marker, self.module):
                ns = self._nspid(name)
                if ns and len(ns) >= 2 and ns[-1] == "1":
                    out.append(int(name))
        return out

    def find_marker_procs(self, marker: str) -> list:
        """Every process whose argv carries the exact tree/marker arguments, init or not (e.g. the
        unshare parent, or something left half-restored). Used to prove nothing is left."""
        return [int(n) for n in sorted((n for n in os.listdir(self.proc) if n.isdigit()), key=int)
                if tree_argv_matches(self._argv(n), marker, self.module)]

    def identity_ok(self, pid: int, marker: str) -> bool:
        ns = self._nspid(str(pid))
        return tree_argv_matches(self._argv(str(pid)), marker, self.module) and bool(ns and len(ns) >= 2 and ns[-1] == "1")

    def terminate(self, pid: int, marker: str, grace_s: float = 10.0, require_init: bool = True):
        """Pin with a pidfd, re-verify identity, then SIGTERM/SIGKILL through the pidfd. -> (ok, reason)

        require_init=False accepts any process with the exact marker argv (second pass over leftovers)
        and goes straight to SIGKILL, because a half-restored or stopped task may ignore SIGTERM."""
        if not self.has_pidfd():
            return False, "pidfd unsupported; refusing to signal"
        try:
            fd = os.pidfd_open(pid)
        except ProcessLookupError:
            return True, "already gone"
        except OSError as e:
            return False, f"pidfd_open failed: {e}"
        try:
            exact = tree_argv_matches(self._argv(str(pid)), marker, self.module)
            if not (self.identity_ok(pid, marker) if require_init else exact):
                return False, "pid does not match the slot's exact argv / pid-1-in-namespace identity"
            if not self._send(fd, signal.SIGTERM if require_init else signal.SIGKILL):
                return True, "exited before the signal"
            if self._wait_gone(pid, grace_s):
                return True, "terminated"
            if not self._send(fd, signal.SIGKILL):
                return True, "exited before SIGKILL"
            if self._wait_gone(pid, self.kill_wait_s):
                return True, "killed"
            return False, f"still alive after SIGKILL (waited {self.kill_wait_s:g} s)"
        finally:
            os.close(fd)

    @staticmethod
    def _send(fd: int, sig) -> bool:
        """Signal the pinned process. False when it has already exited and been reaped: ESRCH on a pidfd means exactly that
        process is gone (the pidfd cannot point at a reused pid), which is what the caller wanted."""
        try:
            signal.pidfd_send_signal(fd, sig)
            return True
        except ProcessLookupError:
            return False

    def _wait_gone(self, pid: int, seconds: float) -> bool:
        deadline = time.monotonic() + seconds
        while True:
            if not os.path.exists(f"{self.proc}/{pid}") or self._zombie(pid):
                return True
            if time.monotonic() >= deadline:
                return False
            time.sleep(0.05)

    def _zombie(self, pid: int) -> bool:
        try:
            with open(f"{self.proc}/{pid}/stat") as f:
                return f.read().rsplit(")", 1)[1].split()[0] == "Z"
        except OSError:
            return True


def sweep_marker(host, marker: str, grace_s: float = 10.0, known_pids=()) -> dict:
    """Terminate EVERY tree carrying this marker (not just a recorded pid), then prove none is left.

    Pass 1: each init (plus any recorded pid) gets verified SIGTERM->SIGKILL. Pass 2: any remaining
    marker-argv process is force-killed after an exact-argv check. `ok` is True only if a final scan
    finds nothing. Failures and leftovers are returned as evidence, never swallowed.
    """
    rep = {"marker": marker, "terminated": [], "force_killed": [], "failures": [], "remaining": [], "ok": False}
    for pid in sorted(set(known_pids) | set(host.find_all_inits(marker))):
        ok, why = host.terminate(pid, marker, grace_s)
        (rep["terminated"] if ok else rep["failures"]).append({"pid": pid, "why": why})
    for pid in host.find_marker_procs(marker):
        ok, why = host.terminate(pid, marker, grace_s, require_init=False)
        (rep["force_killed"] if ok else rep["failures"]).append({"pid": pid, "why": why})
    rep["remaining"] = host.find_marker_procs(marker)
    rep["ok"] = not rep["failures"] and not rep["remaining"]
    return rep
