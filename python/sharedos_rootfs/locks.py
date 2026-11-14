"""File-lock contract for the candidate's checkpoint, read from the pinned CRIU 4.3 and runc 1.5.2 sources.

CRIU (criu/file-lock.c, proc_parse.c): on kernels whose /proc/PID/fdinfo carries a `lock:` line (kdat.has_fdinfo_lock) every lock on an fd of a
dumped task is collected with `real_owner` set, and `dump_file_locks()` then fails with "Some file locks are hold by dumping tasks! You can try
--file-locks to dump them." unless `--file-locks` is given. criu(8): dump `--file-locks` "Dump file locks. It is necessary to make sure that all
file lock users are taken into dump, so it is only safe to use this for enclosed containers where locks are not held by any processes outside of
dumped process tree"; restore `--file-locks` "Restore file locks from the image". runc v1.5.2 forwards `checkpoint|restore --file-locks`
(CriuOpts.FileLocks, default false) to the RPC `FileLocks` at BOTH dump (criu_linux.go:350) and restore (:704): the maintained runtime also
fails closed by default, so the OCI control passing means no lock was held there, not that locks are ignored.

So the flag is safe exactly when the man page's condition holds. This module checks that condition from the host before the dump, read-only:
every lock on an fd of the dumped tree is census'd, and `--file-locks` is allowed only if each lock is a plain FLOCK/POSIX/OFD lock on a file of
the slot's private filesystems and no process outside the tree holds or waits on a lock of the same file. No lock => no flag (the default
fail-closed behaviour stays). Pure functions with injected readers; nothing here changes anything.
"""

from __future__ import annotations

import json
import os
import re
import sys
import time

KINDS = {"POSIX": "posix", "FLOCK": "flock", "OFDLCK": "ofd", "LEASE": "lease"}
ALLOWED_KINDS = ("posix", "flock", "ofd")
PRIVATE_FSTYPES = ("overlay", "tmpfs")
LOCK_RE = re.compile(r"^(?P<id>\d+):\s*(?P<blocked>->\s*)?(?P<flag>\S+)\s+(?P<adv>\S+)\s+(?P<type>\S+)\s+(?P<owner>-?\d+)\s+"
                     r"(?P<maj>[0-9a-fA-F]+):(?P<min>[0-9a-fA-F]+):(?P<ino>\d+)\s+(?P<start>\d+)\s+(?P<end>\S+)\s*$")


class LockError(Exception):
    pass


def parse_lock(text: str) -> dict:
    """One `lock:` value of fdinfo (after `lock:\\t`) or one /proc/locks line."""
    m = LOCK_RE.match(text.strip())
    if not m:
        raise LockError(f"unparseable lock entry: {text.strip()[:120]!r}")
    g = m.groupdict()
    return {"id": int(g["id"]), "kind": KINDS.get(g["flag"], "unknown:" + g["flag"]), "advisory": g["adv"], "type": g["type"], "blocked": bool(g["blocked"]),
            "owner": int(g["owner"]), "dev": f"{int(g['maj'], 16):d}:{int(g['min'], 16):d}", "ino": int(g["ino"]), "start": int(g["start"]), "end": g["end"]}


# ----------------------------------------------------------------------------------------- live census (read-only)
def census(tree_pids, read, listdir, readlink) -> list:
    """Locks on fds of the dumped tasks. read(path)->str, listdir(path)->[names], readlink(path)->str; all injected."""
    out, maps_cache = [], {}
    for pid in tree_pids:
        try:
            fds = sorted(listdir(f"/proc/{pid}/fd"), key=lambda x: int(x))
        except (OSError, ValueError):
            continue                                            # the task exited while being read; CRIU's own view decides at dump time
        for fd in fds:
            try:
                info = read(f"/proc/{pid}/fdinfo/{fd}")
            except OSError:
                continue
            mnt = next((int(l.split()[1]) for l in info.splitlines() if l.startswith("mnt_id:")), None)
            for line in info.splitlines():
                if line.startswith("lock:"):
                    body = line.split(":", 1)[1].strip()
                    if not body:
                        continue
                    rec = parse_lock(body)
                    try:
                        path = readlink(f"/proc/{pid}/fd/{fd}")
                    except OSError:
                        path = None
                    out.append({**rec, "pid": pid, "fd": int(fd), "path": path, "mnt_id": mnt,
                                "mapped_by_holder": _mapped(read, pid, path, rec["dev"], rec["ino"], maps_cache)})
    return out


def _mapped(read, pid, path, dev, ino, cache):
    """Whether the lock holder ALSO maps this file (a file-backed VMA with the same path, or the same dev:ino). CRIU opens the file by its own path
    and caches that fd across consecutive VMAs of the same descriptor/flags, closing it at the end of the chain (VMA_CLOSE); closing any fd of a file drops
    a process's POSIX locks on it, so this is the property that WOULD explain a POSIX lock lost across a restore (a hypothesis, not proven). None = /proc/PID/maps unreadable."""
    if pid not in cache:
        try:
            cache[pid] = read(f"/proc/{pid}/maps").splitlines()
        except OSError:
            cache[pid] = None
    if cache[pid] is None:
        return None
    major, minor = (int(x) for x in dev.split(":"))
    for line in cache[pid]:
        f = line.split(None, 5)
        if len(f) < 5 or f[4] == "0":
            continue
        try:
            mj, mn = (int(x, 16) for x in f[3].split(":"))
        except ValueError:
            continue
        name = f[5].strip() if len(f) > 5 else ""
        if (path and name.removesuffix(" (deleted)") == path.removesuffix(" (deleted)")) or (int(f[4]) == ino and (mj, mn) == (major, minor)):
            return True
    return False


def outside_holders(proc_locks_text: str, tree_pids, files) -> list:
    """Entries of the host's /proc/locks on any of `files` ('maj:min:ino' strings) whose pid is not in the dumped tree (holders AND waiters)."""
    tree, bad = set(tree_pids), []
    for line in proc_locks_text.splitlines():
        if not line.strip():
            continue
        e = parse_lock(line)
        if f"{e['dev']}:{e['ino']}" in files and e["owner"] not in tree:
            bad.append(e)
    return bad


def contract(locks: list, mountinfo_fstypes: dict, outside: list) -> list:
    """Violations (empty = `--file-locks` is safe for this tree). mountinfo_fstypes maps mnt_id -> fstype of the init's mount table."""
    bad = []
    for l in locks:
        where = f"pid {l['pid']} fd {l['fd']} {l['path']}"
        if l["kind"] not in ALLOWED_KINDS:
            bad.append(f"{where}: {l['kind']} locks are not supported by this contract")
        if l["advisory"] == "MSNFS":
            bad.append(f"{where}: a mandatory (MSNFS) lock")
        fs = mountinfo_fstypes.get(l["mnt_id"])
        if fs not in PRIVATE_FSTYPES:
            bad.append(f"{where}: the file is on a {fs!r} mount (not one of the slot's private {PRIVATE_FSTYPES})")
    for e in outside:
        bad.append(f"a process outside the dumped tree (pid {e['owner']}) {'waits for' if e['blocked'] else 'holds'} a lock on {e['dev']}:{e['ino']}")
    return bad


# ----------------------------------------------------------------------------------------- tree membership (shape-independent)
def pid_ns_members(root, listdir=os.listdir, readlink=os.readlink) -> list:
    """Every process in the same PID namespace as `root` (for the slot's init that IS the whole dumped tree, wherever it sits in the ppid
    graph: orphans reparented to init, tasks restored under a different parent, ...)."""
    try:
        mine = readlink(f"/proc/{root}/ns/pid")
    except OSError:
        return []
    out = []
    for n in listdir("/proc"):
        if n.isdigit():
            try:
                if readlink(f"/proc/{n}/ns/pid") == mine:
                    out.append(int(n))
            except OSError:
                continue
    return sorted(out)


def children_walk(root, listdir=os.listdir, read=None) -> list:
    """Descendants through /proc/PID/task/TID/children (the ppid shape)."""
    read = read or (lambda p: open(p).read())
    seen, todo = [], [root]
    while todo:
        cur = todo.pop()
        try:
            tids = listdir(f"/proc/{cur}/task")
        except OSError:
            continue
        for tid in tids:
            try:
                kids = read(f"/proc/{cur}/task/{tid}/children").split()
            except OSError:
                continue
            for k in map(int, kids):
                if k not in seen:
                    seen.append(k)
                    todo.append(k)
    return sorted(seen)


# ----------------------------------------------------------------------------------------- CRIU's own record of what it dumped and applied
KIND_CODE = {"posix": 1, "flock": 2, "ofd": 4, "lease": 8}
TYPE_CODE = {"READ": 0, "WRITE": 1}                                  # F_RDLCK, F_WRLCK as CRIU stores them
DUMP_LINE = re.compile(r"LOCK flag: (\d+),type: (\d+),pid: (-?\d+),fd: (-?\d+),start:\s*([0-9a-f]+),len:\s*([0-9a-f]+)")
APPLY_LINE = re.compile(r"\((posix|flock|ofd|lease)\)flag: (\d+), type: (\d+)(?:, cmd: \d+)?, pid: (-?\d+), fd: (-?\d+)(?:, start:\s*(?:0x)?([0-9a-f]+), len:\s*(?:0x)?([0-9a-f]+))?")


def _norm(flag, typ, pid, fd, start, ln):
    # flock restore lines carry no range, so FLOCK entries are compared without one
    return [flag, typ, pid, fd, None if flag == 2 else start, None if flag == 2 else ln]


def dumped_from_log(text: str) -> list:
    """`LOCK flag:` lines of file-lock.c dump_one_file_lock(): what CRIU wrote to filelocks.img (flag, type, vpid, fd, start, len)."""
    return sorted((_norm(int(m[1]), int(m[2]), int(m[3]), int(m[4]), int(m[5], 16), int(m[6], 16)) for m in DUMP_LINE.finditer(text)), key=str)


def applied_from_log(text: str) -> list:
    """`(posix)flag:` / `(flock)flag:` / `(ofd)flag:` lines of file-lock.c restore_file_lock(): what the restore applied to the tasks."""
    out = []
    for m in APPLY_LINE.finditer(text):
        flag = int(m[2])
        out.append(_norm(flag, int(m[3]), int(m[4]), int(m[5]), int(m[6], 16) if m[6] else None, int(m[7], 16) if m[7] else None))
    return sorted(out, key=str)


def census_tuples(locks: list) -> list:
    """The live census in the same shape as CRIU's record, without the pid (host and namespace pids differ): (flag, type, fd, start, len)."""
    out = []
    for l in locks:
        flag, typ = KIND_CODE.get(l["kind"], -1), TYPE_CODE.get(l["type"], -1)
        length = 0 if l["end"] == "EOF" else int(l["end"]) + 1 - l["start"]
        out.append([flag, typ, l["fd"], None if flag == 2 else l["start"], None if flag == 2 else length])
    return sorted(out, key=str)


def shape_of_dump(entries: list) -> list:
    """CRIU's dumped entries as a SET of (flag, type, start, len). The live census is compared as a set because several fds of one open file each
    show the same lock in fdinfo while CRIU records it once; the exact multiset check (with pid and fd) is applied-vs-dumped, CRIU against CRIU."""
    return sorted({(e[0], e[1], e[4], e[5]) for e in entries}, key=str)


def shape_of_census(locks: list) -> list:
    out = {(KIND_CODE.get(l["kind"], -1), TYPE_CODE.get(l["type"], -1), None if l["kind"] == "flock" else l["start"],
            None if l["kind"] == "flock" else (0 if l["end"] == "EOF" else int(l["end"]) + 1 - l["start"])) for l in locks}
    return sorted(out, key=str)


# ----------------------------------------------------------------------------------------- CRIU action-script hook (criu(8) --action-script)
def hook_main(out_dir: str, env=None, listdir=os.listdir, readlink=os.readlink, read=None, clock=time.time) -> int:
    """Run by CRIU at its `pre-resume` and `post-resume` actions (criu(8): pre-resume = "all processes and resources are restored but tasks are
    stopped waiting for final kick to run"; the script's exit code is only logged, it cannot veto). It writes a read-only census of the restored
    tree to <out_dir>/locks-<action>.json for the arm to compare; it changes nothing."""
    env = os.environ if env is None else env
    action, root = env.get("CRTOOLS_SCRIPT_ACTION"), env.get("CRTOOLS_INIT_PID")
    if action not in ("pre-resume", "post-resume"):
        return 0
    read = read or (lambda p: open(p).read())
    rec = {"action": action, "epoch": clock(), "init_pid": root}
    try:
        pid = int(root)
        by_ns, by_children = pid_ns_members(pid, listdir, readlink), children_walk(pid, listdir, read)
        tree = sorted(set(by_ns) | set(by_children) | {pid})
        locks = census(tree, read, listdir, readlink)
        host_lines = []
        try:
            tree_set = set(tree)
            for line in read("/proc/locks").splitlines():
                if line.strip() and parse_lock(line)["owner"] in tree_set:
                    host_lines.append(line.strip())
        except (OSError, LockError) as e:
            host_lines = [f"unreadable: {e}"]
        rec.update(tree_pids=tree, by_pid_ns=by_ns, by_children=by_children, locks=locks, tuples=census_tuples(locks), shape=shape_of_census(locks),
                   host_proc_locks_of_tree=host_lines)
    except (TypeError, ValueError, OSError, LockError) as e:
        rec["error"] = f"{type(e).__name__}: {e}"
    path = os.path.join(out_dir, f"locks-{action}.json")
    with open(path, "x") as f:
        json.dump(rec, f, indent=2, sort_keys=True)
    return 0


# ----------------------------------------------------------------------------------------- offline: which task held the lock (dump.log)
def scan_log(text: str) -> list:
    """From a CRIU dump.log: every `lockinfo:` line with the task whose fds were being collected ("Collecting fds (pid: N)") and its comm
    ("Dumping task (pid: N comm: C)"). `lockinfo:` is printed as 'id:kind ltype owner maj:min:ino start end' (kind: 1 POSIX, 2 FLOCK, 4 OFD, 8 LEASE)."""
    comm, cur, out = {}, None, []
    kinds = {1: "posix", 2: "flock", 4: "ofd", 8: "lease"}
    for line in text.splitlines():
        m = re.search(r"Dumping task \(pid: (\d+) comm: ([^)]*)\)", line)
        if m:
            comm[int(m.group(1))] = m.group(2)
        m = re.search(r"Collecting fds \(pid: (\d+)\)", line)
        if m:
            cur = int(m.group(1))
        m = re.search(r"lockinfo: (\d+):(-?\d+) ([0-9a-f]+) (-?\d+) ([0-9a-f]+):([0-9a-f]+):(\d+) (\d+) (\S+)", line)
        if m:
            out.append({"collecting_pid": cur, "comm": comm.get(cur), "id": int(m.group(1)), "kind": kinds.get(int(m.group(2)), m.group(2)),
                        "owner": int(m.group(4)), "dev": f"{int(m.group(5), 16)}:{int(m.group(6), 16)}", "ino": int(m.group(7)),
                        "start": int(m.group(8)), "end": m.group(9)})
        if "Found lock entry" in line:
            out.append({"found_lock_entry": line.strip()})
    return out


def find_paths(dev: str, ino: int, roots, limit=20) -> list:
    """Offline best effort: files under `roots` with this st_ino (and st_dev when it matches). Overlay inode numbers may not equal the layer's, so
    results are candidates, never proof."""
    hits = []
    for root in roots:
        for dp, dns, fns in os.walk(root, followlinks=False):
            for n in fns + dns:
                p = os.path.join(dp, n)
                try:
                    st = os.lstat(p)
                except OSError:
                    continue
                if st.st_ino == ino:
                    hits.append({"path": p, "dev_match": f"{os.major(st.st_dev)}:{os.minor(st.st_dev)}" == dev})
                    if len(hits) >= limit:
                        return hits
    return hits


def main(argv) -> int:
    if len(argv) == 2 and argv[0] == "--hook":
        return hook_main(argv[1])
    if not argv or argv[0] in ("-h", "--help"):
        print("usage: python -m sharedos_rootfs.locks DUMP_LOG [--search ROOT ...]   (offline; reads the log, searches the given directories)")
        return 2
    roots = argv[argv.index("--search") + 1:] if "--search" in argv else []
    with open(argv[0]) as f:
        found = scan_log(f.read())
    for e in found:
        if "ino" in e and roots:
            e["candidates"] = find_paths(e["dev"], e["ino"], roots)
    print(json.dumps(found, indent=2))
    return 0 if any("ino" in e for e in found) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
