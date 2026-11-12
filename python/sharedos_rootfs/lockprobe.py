"""Tiny discriminator for the live finding "a POSIX lock on meta.db-shm was missing while the restored tasks were still stopped".

Source reading (CRIU v4.3, independently checked at commit 2b7ac603): cr-restore.c restore_one_alive_task() applies file locks (`prepare_file_locks`:
`fcntl(F_SETLKW)` per POSIX lock) BEFORE `open_vmas()` (lines 659 and 662). For a file-backed VMA, files-reg.c open_filemap() opens the mapping's file
through its own path (not through the task's restored fd) and caches that fd across consecutive VMAs of the same file description and flags (it opens a new
one only when either changes); the last VMA of such a chain is marked VMA_CLOSE, and pie/restorer.c:957-958 closes it right after the mmap. POSIX semantics
(fcntl(2)): closing ANY fd of a file releases ALL of the calling process's POSIX locks on it. So a POSIX lock on a file the same task also maps (SQLite's WAL
index meta.db-shm) would plausibly be released by that close, while a lock on an unmapped file (meta.db) survives; flock/OFD locks belong to the open file
description and would survive. This is a reading of the source, not a proof: the probe below is what tests it. This module separates the hypotheses without GNOME:

  kernel : two fds on one file; lock through the first, open+mmap+close the second; read the host's /proc/locks and the fdinfo census before and after.
           Pure kernel semantics, no CRIU, no root needed.
  criu   : (operator, root) a tiny process holds POSIX read locks on file A (plain) and file B (also mmapped), is dumped with `--file-locks` and restored
           with `--file-locks --action-script`; the pre-resume census (fdinfo AND the host's /proc/locks) is compared with the census before the dump.
           Expected if the hypothesis holds: A kept, B lost. Both kept: the hypothesis is false and the cause is elsewhere (kernel/procfs visibility).
Nothing here repairs anything or changes a flag. Evidence goes to a NEW directory; only the probe's own processes are ever signalled (identity checked).
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import mmap
import os
import subprocess
import sys
import time

try:
    from . import locks as lk
except ImportError:                                   # also runnable as a script by the CRIU action hook's neighbour
    import locks as lk


def proc_locks_for(path: str) -> list:
    """The host's /proc/locks entries for this file (matched by device:inode of the file itself)."""
    st = os.stat(path)
    dev = f"{os.major(st.st_dev)}:{os.minor(st.st_dev)}"
    out = []
    with open("/proc/locks") as f:
        for line in f:
            if line.strip():
                e = lk.parse_lock(line)
                if e["ino"] == st.st_ino and e["dev"] == dev:
                    out.append({"kind": e["kind"], "type": e["type"], "owner": e["owner"], "start": e["start"], "end": e["end"]})
    return out


def kernel_phase(workdir: str) -> dict:
    """In-process, no CRIU. Returns what the kernel did to a POSIX lock (and a flock, for contrast) when a SECOND fd of the same file is mapped and closed."""
    res = {"kernel": os.uname().release}
    for kind in ("posix", "flock"):
        path = os.path.join(workdir, f"kernel-{kind}.db")
        with open(path, "wb") as f:
            f.write(b"\0" * 8192)
        locked = os.open(path, os.O_RDWR)
        if kind == "posix":
            fcntl.lockf(locked, fcntl.LOCK_SH, 1, 128, 0)            # byte 128 read lock, like SQLite's shm lock bytes
        else:
            fcntl.flock(locked, fcntl.LOCK_SH)
        before = proc_locks_for(path)
        second = os.open(path, os.O_RDWR)
        mm = mmap.mmap(second, 4096, mmap.MAP_SHARED, mmap.PROT_READ)    # a fresh fd, mapped, then closed: what CRIU's restorer does
        os.close(second)
        after_map_close = proc_locks_for(path)
        mm.close()
        os.close(locked)
        res[kind] = {"before": before, "after_closing_a_second_fd": after_map_close, "lock_survived": len(after_map_close) == len(before) > 0}
    res["conclusion"] = ("POSIX lock released by closing a second fd of the same file; flock survived" if not res["posix"]["lock_survived"] and res["flock"]["lock_survived"]
                         else "POSIX lock survived the close of a second fd: the restorer's close does not explain a lost POSIX lock on this kernel"
                         if res["posix"]["lock_survived"] else "unexpected: neither lock survived")
    return res


CHILD = r"""
import fcntl, mmap, os, sys, time
d = sys.argv[1]
a = os.open(os.path.join(d, "A.db"), os.O_RDWR | os.O_CREAT, 0o600)       # plain file: lock only
b = os.open(os.path.join(d, "B.db-shm"), os.O_RDWR | os.O_CREAT, 0o600)   # file that is also mapped, like SQLite's WAL index
os.ftruncate(a, 8192); os.ftruncate(b, 8192)
fcntl.lockf(a, fcntl.LOCK_SH, 510, 1073741826, 0)
fcntl.lockf(b, fcntl.LOCK_SH, 1, 128, 0)
mm = mmap.mmap(b, 4096, mmap.MAP_SHARED, mmap.PROT_READ | mmap.PROT_WRITE)
fd = os.open(os.devnull, os.O_RDWR)
for n in (0, 1, 2):
    os.dup2(fd, n)
open(os.path.join(d, "ready"), "w").close()
while True:
    time.sleep(3600)
"""


class Probe:
    def __init__(self, evidence, run=None, popen=None, criu="criu", clock=time.time, sleep=time.sleep, start_ticks=None, wait_s=30.0, cmdline=None,
                 kill=None):
        self.ev, self.criu, self.clock, self.sleep, self.wait_s = evidence, criu, clock, sleep, wait_s
        self.cmdline, self.kill = cmdline or _cmdline, kill or os.kill
        self.run = run or self._run
        self.popen = popen or subprocess.Popen
        self.start_ticks = start_ticks or _ticks
        self.owned = {}                               # pid -> start ticks of processes this probe created (the only ones it may signal)

    @staticmethod
    def _run(argv, timeout=300):
        r = subprocess.run(argv, capture_output=True, text=True, timeout=timeout)
        return r.returncode, r.stdout, r.stderr

    def _census(self, pid, files):
        locks = lk.census([pid], lambda p: open(p).read(), os.listdir, os.readlink)
        return {"fdinfo": [{k: l[k] for k in ("fd", "kind", "type", "start", "end", "path", "mapped_by_holder")} for l in locks],
                "host_proc_locks": {name: proc_locks_for(path) for name, path in files.items()}}

    def _own(self, pid):
        t = self.start_ticks(pid)
        if t is None:
            raise RuntimeError(f"pid {pid} is not alive: cannot take ownership")
        self.owned[pid] = t

    def _stop(self, pid):
        t = self.owned.get(pid)
        if t is None or self.start_ticks(pid) != t:
            return f"pid {pid} is gone or not the probe's process: not signalled"
        try:
            self.kill(pid, 9)
        except ProcessLookupError:
            pass
        return None

    def _attempt(self, argv, timeout):
        """(rc, stdout, stderr, timed_out): a timeout is a result, not an escape: the caller still adopts and stops what the command left behind."""
        try:
            rc, so, se = self.run(argv, timeout)
            return rc, so, se, False
        except subprocess.TimeoutExpired:
            return None, "", "timed out", True

    def _criu_identity(self):
        """The criu binary actually used: resolved path, sha256 and its own --version text (a patched build must never pass for the stock one)."""
        import shutil
        path = shutil.which(self.criu) or self.criu
        try:
            with open(path, "rb") as f:
                sha = hashlib.sha256(f.read()).hexdigest()
            ver = self.run([self.criu, "--version"], 20)[1].strip()
        except Exception as e:      # noqa: BLE001  (identity is evidence; a fake runner or a missing binary just records why it is absent)
            return {"path": path, "error": f"{type(e).__name__}: {e}"}
        return {"path": path, "sha256": sha, "version": ver}

    def _reap(self, child, timeout=30.0):
        """`criu dump` kills the dumped task, which stays a ZOMBIE (still holding its pid) until its parent waits: a restore under the original pid then
        fails with "File exists". The probe is that parent, so it reaps the child before the restore, bounded; the reaped pid is no longer owned."""
        try:
            rc = child.wait(timeout=timeout)
        except subprocess.TimeoutExpired:
            raise RuntimeError(f"the dumped child (pid {child.pid}) was not reaped within {timeout}s: not restoring under a live pid")
        self.owned.pop(child.pid, None)
        return rc

    def _adopt_partial(self, pid, original_ticks, child_argv):
        """After a failed or timed-out restore: CRIU restores a task under its ORIGINAL pid. A process now at that pid is the probe's partially restored
        tree only if its argv is exactly the child's and it is a NEW incarnation (different start ticks). Adopted -> recorded and stopped with the other
        owned processes. Anything else at that pid (another program, pid reuse) is neither adopted nor signalled."""
        ticks, cmd = self.start_ticks(pid), self.cmdline(pid)
        rec = {"pid": pid, "start_ticks": ticks, "cmdline_matches": cmd == child_argv}
        if ticks is None:
            rec["state"] = "no process at the original pid: nothing partial to stop"
        elif ticks == original_ticks:
            rec["state"] = "that is still the original incarnation (already owned)"
        elif cmd == child_argv:
            self.owned[pid] = ticks
            rec["state"] = "partially restored incarnation adopted (it will be stopped)"
        else:
            rec["state"] = "a different program holds that pid: not adopted, not signalled"
        return rec

    def criu_phase(self, hook_script_path_factory=None) -> dict:
        d = os.path.join(self.ev, "criu-lock")
        for sub in ("files", "images", "hook"):
            os.makedirs(os.path.join(d, sub))
        files = {"A (plain, locked)": os.path.join(d, "files", "A.db"), "B (locked and mapped)": os.path.join(d, "files", "B.db-shm")}
        child_argv = [sys.executable, "-c", CHILD, os.path.join(d, "files")]
        res = {"phase": "criu", "source_sha256": source_hashes(), "criu_binary": self._criu_identity()}
        p = self.popen(child_argv, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
        orig_pid, orig_ticks = p.pid, None
        try:
            self._own(p.pid)                              # owned the moment it exists, BEFORE any wait that could time out
            orig_ticks = self.owned[p.pid]
            t0 = self.clock()
            while not os.path.exists(os.path.join(d, "files", "ready")):
                if self.clock() - t0 > self.wait_s:
                    raise RuntimeError("the probe child never became ready")
                self.sleep(0.1)
            res["before_dump"] = self._census(p.pid, files)
            hook = os.path.join(d, "hook", "hook.sh")
            with open(hook, "x") as f:
                f.write("#!/bin/sh\nexec %s -I %s --hook %s\n" % (sys.executable, os.path.join(os.path.dirname(os.path.abspath(__file__)), "locks.py"),
                                                                 os.path.join(d, "hook")))
            os.chmod(hook, 0o700)
            rc, so, se, to = self._attempt([self.criu, "dump", "-t", str(p.pid), "-D", os.path.join(d, "images"), "-o", os.path.join(d, "dump.log"), "-v4",
                                            "--file-locks", "--shell-job"], 300)
            res["dump_rc"] = rc
            if to or rc != 0:
                raise RuntimeError("criu dump timed out" if to else f"criu dump failed rc={rc}: {se[-300:]}")
            res["dumped_log_locks"] = lk.dumped_from_log(_read(os.path.join(d, "dump.log")))
            res["original_reaped_rc"] = self._reap(p)      # BEFORE the restore reuses its pid
            pidfile = os.path.join(d, "restored.pid")
            rc, so, se, to = self._attempt([self.criu, "restore", "-D", os.path.join(d, "images"), "-o", os.path.join(d, "restore.log"), "-v4", "--file-locks",
                                            "--restore-detached", "--shell-job", "--pidfile", pidfile, "--action-script", hook], 300)
            res["restore_rc"] = rc
            if to or rc != 0:
                res["partial_restore"] = self._adopt_partial(orig_pid, orig_ticks, child_argv)
                if os.path.isfile(os.path.join(d, "restore.log")):
                    res["restore_log_tail"] = _read(os.path.join(d, "restore.log"))[-1500:]
                raise RuntimeError("criu restore timed out" if to else f"criu restore failed rc={rc}: {se[-300:]}")
            rpid = int(_read(pidfile))
            self._own(rpid)
            res["applied_log_locks"] = lk.applied_from_log(_read(os.path.join(d, "restore.log")))
            try:
                with open(os.path.join(d, "hook", "locks-pre-resume.json")) as f:
                    res["pre_resume"] = json.load(f)
            except (OSError, ValueError) as e:
                res["pre_resume"] = {"error": f"no readable pre-resume census: {e}"}
            res["after_resume"] = self._census(rpid, files)
            res["verdict"] = verdict(res)
        except BaseException as e:
            res["error"] = f"{type(e).__name__}: {e}"
            raise
        finally:
            res["cleanup_problems"] = [m for m in (self._stop(pid) for pid in list(self.owned)) if m]
            res["stopped_pids"] = sorted(self.owned)
            try:
                with open(os.path.join(d, "criu-phase-result.json"), "w") as f:       # kept even when the phase raised
                    json.dump(res, f, indent=2, sort_keys=True, default=str)
            except OSError:
                pass
        return res


def _cmdline(pid):
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return f.read().decode(errors="replace").split("\0")[:-1]
    except OSError:
        return None


def source_hashes() -> dict:
    here = os.path.dirname(os.path.abspath(__file__))
    out = {n: hashlib.sha256(open(os.path.join(here, n), "rb").read()).hexdigest() for n in ("lockprobe.py", "locks.py")}
    out["child_source"] = hashlib.sha256(CHILD.encode()).hexdigest()
    return out


def _ticks(pid):
    try:
        with open(f"/proc/{pid}/stat") as f:
            raw = f.read()
        return raw[raw.rindex(")") + 2:].split()[19]
    except (OSError, ValueError, IndexError):
        return None


def _read(path):
    with open(path) as f:
        return f.read()


def verdict(res: dict) -> str:
    """Compares the lock set CRIU dumped with what the restored tasks held while still stopped (the pre-resume census)."""
    pre = res.get("pre_resume") or {}
    if pre.get("error"):
        return f"inconclusive: {pre['error']}"
    held = {l["path"] for l in pre.get("locks", [])}
    a_kept = any(p.endswith("A.db") for p in held)
    b_kept = any(p.endswith("B.db-shm") for p in held)
    if len(res.get("dumped_log_locks", [])) != 2:
        return f"inconclusive: CRIU dumped {len(res.get('dumped_log_locks', []))} locks, expected 2"
    host = pre.get("host_proc_locks_of_tree")
    if a_kept and not b_kept:
        return ("CONFIRMED mechanism: the lock on the plain file survived and the lock on the file the task also maps is missing while the tasks are "
                f"stopped (the restorer closes the VMA fd after the locks were applied); the host's /proc/locks for the tree shows {host}")
    if a_kept and b_kept:
        return "NOT REPRODUCED with this criu binary: both locks are present before resume (compare criu_binary.sha256 with the stock run: this is the expected result of a patched build, and a failure to reproduce for a stock build)"
    return "UNEXPECTED: the plain file's lock is missing too"


def main(argv) -> int:
    ap = __import__("argparse").ArgumentParser(prog="lockprobe")
    ap.add_argument("--evidence", required=True, help="a NEW directory")
    ap.add_argument("--phases", default="kernel")
    ap.add_argument("--criu", default="criu", help="the criu binary to dump/restore with (its sha256 and --version are recorded)")
    a = ap.parse_args(argv)
    if os.path.lexists(a.evidence):
        print(f"lockprobe: {a.evidence} already exists: use a NEW directory", file=sys.stderr)
        return 2
    os.makedirs(a.evidence, mode=0o700)
    out = {}
    rc = 0
    if "kernel" in a.phases.split(","):
        out["kernel"] = kernel_phase(a.evidence)
    if "criu" in a.phases.split(","):
        if os.geteuid() != 0:
            print("lockprobe: the criu phase needs root", file=sys.stderr)
            return 2
        try:
            out["criu"] = Probe(a.evidence, criu=a.criu).criu_phase()
            rc = 0 if not out["criu"]["cleanup_problems"] else 1
        except (RuntimeError, OSError, ValueError) as e:
            out["criu"] = {"verdict": f"phase failed: {e}", "see": os.path.join(a.evidence, "criu-lock", "criu-phase-result.json")}
            rc = 1
    with open(os.path.join(a.evidence, "lockprobe-result.json"), "w") as f:
        json.dump(out, f, indent=2, sort_keys=True, default=str)
    print(json.dumps({k: (v.get("conclusion") or v.get("verdict")) for k, v in out.items()}, indent=2))
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
