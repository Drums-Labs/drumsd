"""Command construction for one rootfs slot. Pure functions: nothing here touches the system.

All CRIU flags below follow criu(8) and were NOT executed against a real CRIU for this tree; whether they are
sufficient for an overlay-rooted, chrooted desktop with a Flask server is exactly what the first real run measures.
"""

from __future__ import annotations

import hashlib
import os
import time
import sys

TREE_MODULE = "sharedos_rootfs.tree"
PROXY_MODULE = "sharedos_rootfs.proxy"


def marker_for(state_dir: str, slot: int) -> str:
    return f"rootfs-{hashlib.sha256(os.path.abspath(state_dir).encode()).hexdigest()[:8]}-{slot}"


def slot_paths(state_dir: str, slot: int) -> dict:
    d = os.path.join(os.path.abspath(state_dir), f"slot{slot}")
    return {"dir": d, "upper": os.path.join(d, "upper"), "work": os.path.join(d, "work"),
            "merged": os.path.join(d, "merged"), "images": os.path.join(d, "images"),
            "plan": os.path.join(d, "service-plan.json"), "tree_log": os.path.join(d, "tree.log"),
            "proxy_log": os.path.join(d, "proxy.log"), "slot_json": os.path.join(d, "slot.json")}


def cgroup_properties(vcpus: int, memory_mib: int) -> list:
    """Equal-resources attempt: N vCPU -> CPU quota N*100%, memory -> MemoryMax, no swap. Approximate by nature."""
    return [f"CPUQuota={vcpus * 100}%", f"MemoryMax={memory_mib}M", "MemorySwapMax=0", "TasksMax=8192"]


def scope_prefix(unit: str, vcpus: int, memory_mib: int) -> tuple:
    props = [x for p in cgroup_properties(vcpus, memory_mib) for x in ("-p", p)]
    return ("systemd-run", "--scope", "--collect", "--quiet", "--unit", unit, *props, "--")


def launch_argv(marker: str, plan_file: str, log: str, vcpus: int, memory_mib: int, slot: int = 0, harden: bool = True) -> tuple:
    """The slot runs in its own USER namespace (harden.userns_unshare_flags) in addition to pid/mount/net/ipc/uts,
    so slot root maps to an unprivileged host uid and root-in-slot is not root-on-host. The user namespace is named
    first on the unshare line so the pid/mount/net/ipc/uts namespaces are owned by it. no_new_privs, the capability
    bounding-set drop and the seccomp filter are applied by the init itself (tree.bring_up -> harden.apply_runtime_
    hardening); they cannot be expressed as unshare flags. `slot` selects the disjoint host sub-uid range.
    harden=False (config "harden": false, trusted-only hosts; harden.enabled): no user namespace, as before the hardening."""
    from . import harden as H
    return (*scope_prefix(f"drumsbench-{marker}.scope", vcpus, memory_mib),
            "setsid", "-f", "sh", "-c", 'exec "$@" </dev/null >>"$0" 2>&1', log,
            "unshare", *(H.userns_unshare_flags(slot) if harden else ()),
            "--pid", "--fork", "--mount", "--net", "--ipc", "--uts", "--propagation", "private",
            sys.executable, "-m", TREE_MODULE, "--marker", marker, "--plan", plan_file)


def proxy_argv(marker: str, init_pid: int, listen_port: int, target_port: int, log: str) -> tuple:
    return ("setsid", "-f", "sh", "-c", 'exec "$@" </dev/null >>"$0" 2>&1', log,
            sys.executable, "-m", PROXY_MODULE, "--marker", marker, "--init-pid", str(init_pid),
            "--listen-port", str(listen_port), "--target-port", str(target_port))


# --nested-pidns (drums-criu branch nested-ns): Chrome, VS Code (Electron) and other sandboxed apps make pid, user and net
# namespaces nested below the slot's own; without the flag criu refuses them ("Can't dump nested pid namespace"). On restore
# the nested namespaces are recreated from the images either way; the flag is passed there too so dump and restore name it.
CRIU_NESTED = ("--nested-pidns",)
CRIU_COMMON = ("--shell-job", "--tcp-close", "--manage-cgroups=ignore", *CRIU_NESTED, "-v4")

# Dump only. An open file whose name is gone (dconf-service replaces ~/.config/dconf/user by rename while every GNOME client
# still maps the old inode) cannot be dumped by name. On overlayfs fstat() can report st_nlink=1 for it, so criu takes the
# linked-remap path (criu/files-reg.c check_path_remap -> dump_linked_remap) and, without this flag, fails the whole dump with
# "Can't create link remap ... Use link-remap option". With it, criu links the inode to a temporary name next to the old one
# (or falls back to a ghost copy when linkat() fails on overlayfs); the name is part of the upper layer copied after the dump
# and criu restore removes it once the files are open again. Restore needs no flag for this.
#
# --ghost-limit: Chrome and Electron keep shared memory in deleted /dev/shm files (4 MiB and up) mapped by several processes;
# criu saves a deleted file's content in the images and refuses any above the limit (default 1 MiB: "Can't dump ghost file
# /dev/shm/.com.google.Chrome.* of 4198400 size, increase limit").
CRIU_GHOST_LIMIT = "512M"
#
# --restart-interrupted-syscalls (drums-criu): a poll()/nanosleep()/futex wait with a timeout that the dump interrupts is made again
# instead of returning EINTR, in the dumped tree (--leave-running) and in every restored one. The file watcher of VS Code takes EINTR
# from poll() for an error and VS Code replaces the process after every dump ("Unable to poll: Interrupted system call").
CRIU_DUMP_ONLY = ("--link-remap", f"--ghost-limit={CRIU_GHOST_LIMIT}", "--restart-interrupted-syscalls")


def dump_argv(pid: int, images: str, log: str, leave_running: bool = False, file_locks: bool = False, externals: tuple = ()) -> tuple:
    """leave_running=False (default): the tree is stopped by the dump, so the overlay upper layer can be copied
    consistently before the tree is restored (CRIU does not snapshot disk state; the upper dir is the disk state).
    externals: the slot's external mounts named by key, from its own mount table (criu_root.dump_externals): the host device binds of a
    slot in its own user namespace and the per-env view binds."""
    return ("criu", "dump", "-t", str(pid), "-D", images, *(["--leave-running"] if leave_running else []),
            *(["--file-locks"] if file_locks else []), *externals, *CRIU_DUMP_ONLY, *CRIU_COMMON, "-o", log)


def restore_argv(images: str, pidfile: str, log: str) -> tuple:
    from . import harden
    return ("criu", "restore", "-D", images, "--restore-detached", *harden.criu_external_restore_args(), *CRIU_COMMON, "--pidfile", pidfile,
            "-o", log)


def restore_scoped_argv(marker: str, generation: int, vcpus: int, memory_mib: int, images: str, pidfile: str, log: str) -> tuple:
    """CRIU restore INSIDE a fresh constrained scope. The restored tasks are forked from criu and, with
    --manage-cgroups=ignore, stay in criu's cgroup, so running criu inside the scope is what keeps them under the
    4 vCPU / 4 GiB limits. A bare `criu restore` would leave them in whatever cgroup the caller is in."""
    return (*scope_prefix(f"drumsbench-{marker}-r{generation}.scope", vcpus, memory_mib), *restore_argv(images, pidfile, log))


def expected_limits(vcpus: int, memory_mib: int) -> dict:
    return {"cpu_max": f"{vcpus * 100000} 100000", "memory_max": str(memory_mib * 1024 * 1024), "swap_max": "0"}


def _read(path: str) -> str:
    with open(path) as f:
        return f.read().strip()


def _cgroup_path(pid: int, proc: str) -> str:
    lines = [x for x in _read(f"{proc}/{pid}/cgroup").splitlines() if x.startswith("0::")]
    if len(lines) != 1:
        raise OSError(f"pid {pid} has no unified (cgroup v2) membership: {lines!r}")
    return lines[0][3:]


def _gone(proc: str, pid: int) -> bool:
    return not os.path.exists(f"{proc}/{pid}")


def _walk(pid: int, proc: str):
    """(descendant pids, vanished count). A pid that exits between being listed and being read is counted as vanished and
    skipped. So is a THREAD of a live process that exits between listdir(task) and the read of its children file: its
    children move to another thread, so the snapshot is incomplete and must be retried, not trusted and not fatal. Any OTHER
    read failure (e.g. a kernel without the children file) raises, so the check still fails closed."""
    out, todo, vanished = [], [pid], 0
    while todo:
        cur = todo.pop()
        try:
            tids = os.listdir(f"{proc}/{cur}/task")
            kids_all = []
            for tid in tids:
                try:
                    kids_all += _read(f"{proc}/{cur}/task/{tid}/children").split()
                except OSError:
                    if not os.path.exists(f"{proc}/{cur}/task/{tid}"):
                        vanished += 1          # that thread exited mid-read; the process itself is still checked below
                        continue
                    raise
        except OSError:
            if cur != pid and _gone(proc, cur):
                vanished += 1
                continue
            raise
        for k in kids_all:
            if int(k) not in out:
                out.append(int(k))
                todo.append(int(k))
    return out, vanished


def _start_time(pid: int, proc: str) -> str:
    """Field 22 of /proc/PID/stat (start time in ticks): with the pid it identifies one process incarnation."""
    return _read(f"{proc}/{pid}/stat").rsplit(")", 1)[1].split()[19]


def _clean_snapshot(pid: int, proc: str):
    """Descendants and their cgroups from a snapshot in which every listed pid was readable. Short-lived children that
    exit mid-read are retried away, but a descendant that is alive in the snapshot is always reported."""
    desc, vanished = _walk(pid, proc)
    cgs = {}
    for d in desc:
        try:
            cgs[d] = _cgroup_path(d, proc)
        except OSError:
            if _gone(proc, d):
                vanished += 1
                continue
            raise
    return desc, cgs, vanished


def read_cgroup(pid: int, proc: str = "/proc", sysfs: str = "/sys/fs/cgroup", attempts: int = 8, settle_s: float = 0.1,
                sleep=time.sleep) -> dict:
    """What the kernel says about pid's cgroup and its limits. Raises OSError when anything cannot be read, when init is
    not the same process throughout, or when no clean snapshot of its descendants could be taken in `attempts` tries."""
    init_start = _start_time(pid, proc)
    path = _cgroup_path(pid, proc)
    d = os.path.join(sysfs, path.lstrip("/"))
    info = {"path": path, "cpu_max": _read(os.path.join(d, "cpu.max")), "memory_max": _read(os.path.join(d, "memory.max")),
            "swap_max": _read(os.path.join(d, "memory.swap.max"))}
    for n in range(attempts):
        procs = [int(x) for x in _read(os.path.join(d, "cgroup.procs")).split()]
        desc, cgs, vanished = _clean_snapshot(pid, proc)
        if vanished == 0:
            if _start_time(pid, proc) != init_start or _cgroup_path(pid, proc) != path:
                raise OSError(f"init pid {pid} changed identity or cgroup while it was being read")
            return {**info, "procs": procs, "descendants": desc, "descendant_cgroups": {str(k): v for k, v in cgs.items()},
                    "init_start_ticks": init_start, "snapshot_attempts": n + 1}
        sleep(settle_s)
    raise OSError(f"no clean snapshot of pid {pid}'s descendants in {attempts} attempts (processes kept exiting mid-read)")


def verify_cgroup(info: dict, pid: int, marker: str, vcpus: int, memory_mib: int) -> list:
    """Problems (empty = verified) between what was readback and the preregistered limits."""
    want, bad = expected_limits(vcpus, memory_mib), []
    name = os.path.basename(info["path"])
    if not (name.startswith(f"drumsbench-{marker}") and name.endswith(".scope")):
        bad.append(f"cgroup {info['path']!r} is not this slot's scope (drumsbench-{marker}*.scope)")
    for k in ("cpu_max", "memory_max", "swap_max"):
        if info[k] != want[k]:
            bad.append(f"{k} is {info[k]!r}, expected {want[k]!r}")
    if pid not in info["procs"]:
        bad.append(f"init pid {pid} is not listed in the scope's cgroup.procs")
    stray = {p: c for p, c in info["descendant_cgroups"].items() if c != info["path"]}
    if stray:
        bad.append(f"descendants outside the constrained cgroup: {stray}")
    return bad
