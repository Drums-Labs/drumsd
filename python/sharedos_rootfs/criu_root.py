"""The CRIU restore-root contract of the custom (candidate) runtime, derived from the pinned sources. Pure functions: nothing here touches
the system.

The reading of the live failure (CRIU v4.3, criu/mount.c, read at the pinned tag): `criu restore` without `--root` takes `prepare_mnt_ns()`'s
`if (!opts.root)` branch, which `collect_mntinfo()`s the mount table the restored namespace inherited from criu's own namespace (here the
HOST's: securityfs, cgroup2, pstore, efivarfs, bpf, debugfs, tracefs ...) and unmounts it through `do_umount_one()` -> `service_mountpoint()`.
In mount-v2 mode (the default when the kernel has move_mount(SET_GROUP) and openat2, as on the 7.0 host) `service_mountpoint()` does
`BUG_ON(!mi->plain_mountpoint)` (mount.c:48), and those freshly collected entries have no plain mountpoint: the BUG. So restoring a mount
namespace without `--root` is not a supported path in mount-v2 (criu(8): `--root` "is required to restore a mount namespace"; in
mount-v2.c `do_mount_root_v2` the container's root mount IS `mount(opts.root, ..., MS_BIND)`).

The contract the maintained runtime (runc v1.5.2 `libcontainer/criu_linux.go` Restore) follows, and this one adopts:
  * `--root` names a directory that is a MOUNT POINT whose PARENT is not overmounted; runc bind-mounts the container rootfs to
    `<state>/criu-root` for that. Here the root is a fresh overlay mount (same lower / upper / work as the slot) made inside a PRIVATE mount
    namespace created for the restore (`unshare --mount --propagation private`), so nothing is mounted in the host's namespace.
  * Mounts the container had from the host are EXTERNAL (`--external mnt[...]`, runc's bind mounts and default devices). The custom tree
    has none by construction (private overlay root, fresh proc/sysfs/devpts/tmpfs, devices created natively on a private tmpfs), and the
    dump-time `mount_contract()` verifies that against the init's own mountinfo, so no external mount, device map or `--root`-less
    fallback is needed or used.
  * `--manage-cgroups=ignore` at BOTH dump and restore, criu itself running inside the slot's constrained scope.
  * The network namespace is NOT given to CRIU as empty/external: criu dumps and restores it itself (no `--empty-ns`, no `--external net`).
`--file-locks` is added to BOTH dump and restore only when the pre-dump lock census (locks.py) shows locks that satisfy criu(8)'s enclosure condition;
with no lock the flag is absent and CRIU keeps failing closed on any lock it finds.
No `--mntns-compat-mode`, no filesystem-ignore or validation-skipping flag is used.
"""

from __future__ import annotations

import re

from . import launch as L

# What a custom-tree mount table may contain: only pseudo/private filesystems rooted at "/" of their own superblock. A host bind mount
# (root != "/"), a device node bind, devtmpfs, cgroup/bpf/securityfs from the host or any disk filesystem would be an EXTERNAL mount that this
# restore has no mapping for, so the dump is refused before it is made.
ALLOWED_FSTYPES = ("overlay", "proc", "sysfs", "tmpfs", "devpts")

# The per-env memory/CPU view (drumsd procview): read-only binds of files of the host's procview FUSE mount over the slot's
# /proc and /sys files, so apps see the slot's own limits. They are the ONLY external mounts a slot has: each is named to CRIU
# at dump (`--external mnt[MOUNTPOINT]:KEY`) and re-bound from the same host file at restore (`--external mnt[KEY]:PATH`).
# The view answers by the reading task's cgroup, so a restored or forked tree reads its own slot's values.
PROCVIEW_FSTYPE = "fuse.drums-procview"
PROCVIEW_FILES = {"meminfo": "/proc/meminfo", "cpuinfo": "/proc/cpuinfo", "stat": "/proc/stat", "swaps": "/proc/swaps",
                  "cpu_online": "/sys/devices/system/cpu/online", "cpu_possible": "/sys/devices/system/cpu/possible",
                  "cpu_present": "/sys/devices/system/cpu/present"}


def procview_key(name: str) -> str:
    return f"drums-pv-{name}"


def is_procview_row(mountpoint: str, fstype: str, root: str) -> bool:
    name = root.lstrip("/")
    return fstype == PROCVIEW_FSTYPE and PROCVIEW_FILES.get(name) == mountpoint and root == "/" + name


def procview_dump_externals(text: str) -> tuple:
    """`--external mnt[MOUNTPOINT]:KEY` for every procview bind in the init's mount table."""
    out = []
    for m in parse_mountinfo(text):
        if is_procview_row(m["mountpoint"], m["fstype"], m["root"]):
            out += ["--external", f"mnt[{m['mountpoint']}]:{procview_key(m['root'].lstrip('/'))}"]
    return tuple(out)


def procview_restore_externals(rows: list, procview_dir: str) -> tuple:
    """`--external mnt[KEY]:PATH` for the procview binds of a dumped mount table (mounts-pre-dump.json rows)."""
    out = []
    for mp, fstype, root in rows:
        if is_procview_row(mp, fstype, root):
            name = root.lstrip("/")
            out += ["--external", f"mnt[{procview_key(name)}]:{procview_dir.rstrip('/')}/{name}"]
    return tuple(out)


def dump_externals(text: str) -> tuple:
    """Every external mount of the init's mount table, named by key: the host device binds of a slot in its own user namespace
    (harden.py) and the per-env view binds. A slot without them (no user namespace, no view) has none."""
    from . import harden
    devs = [m["mountpoint"][len("/dev/"):] for m in parse_mountinfo(text) if is_dev_bind(m)]
    return tuple(x for n in harden.DEV_BIND_NODES if n in devs for x in ("--external", f"mnt[/dev/{n}]:{harden.criu_dev_key(n)}")) \
        + procview_dump_externals(text)


def restore_externals(rows: list, procview_dir: str) -> tuple:
    """The restore side of dump_externals, from the dumped mount table (mounts-pre-dump.json rows [mountpoint, fstype, root])."""
    from . import harden
    devs = [mp[len("/dev/"):] for mp, fstype, root in rows if is_dev_bind({"mountpoint": mp, "fstype": fstype, "root": root})]
    return tuple(x for n in harden.DEV_BIND_NODES if n in devs for x in ("--external", f"mnt[{harden.criu_dev_key(n)}]:/dev/{n}")) \
        + procview_restore_externals(rows, procview_dir)


_OVERLAY_BAD = re.compile(r"[,:\s]")


class ContractError(Exception):
    pass


def parse_mountinfo(text: str) -> list:
    out = []
    for line in text.splitlines():
        if not line.strip():
            continue
        pre, sep, post = line.partition(" - ")
        f, g = pre.split(), post.split()
        if not sep or len(f) < 6 or len(g) < 2:
            raise ContractError(f"unparseable mountinfo line: {line[:120]!r}")
        out.append({"id": int(f[0]), "parent": int(f[1]), "root": f[3], "mountpoint": f[4], "opts": f[5], "fstype": g[0], "source": g[1]})
    return out


def mount_contract(text: str) -> list:
    """Violations (empty = the init's mount table is restorable with no external mounts). Called on /proc/<init>/mountinfo."""
    bad = []
    mounts = parse_mountinfo(text)
    roots = [m for m in mounts if m["mountpoint"] == "/"]
    if len(roots) != 1 or roots[0]["fstype"] != "overlay":
        bad.append(f"the namespace root must be exactly one overlay mount, got {[(m['mountpoint'], m['fstype']) for m in roots]}")
    for m in mounts:
        if is_dev_bind(m):
            continue
        if is_procview_row(m["mountpoint"], m["fstype"], m["root"]):
            if "ro" not in m["opts"].split(","):
                bad.append(f"{m['mountpoint']}: the procview bind is not read-only")
            continue
        if m["fstype"] not in ALLOWED_FSTYPES:
            bad.append(f"{m['mountpoint']}: filesystem {m['fstype']!r} is not one of {ALLOWED_FSTYPES} (it would need an external mount)")
        elif m["root"] != "/":
            bad.append(f"{m['mountpoint']}: {m['fstype']} mount has root {m['root']!r} (a bind of a host path: an external mount)")
    return bad


def is_dev_bind(m: dict) -> bool:
    """One of the standard character devices bound from the host's devtmpfs onto the same name in the slot's /dev (a slot in its own
    user namespace cannot have native nodes). Each is an external mount that dump and restore name explicitly (harden.py)."""
    from . import harden
    n = m["mountpoint"][len("/dev/"):] if m["mountpoint"].startswith("/dev/") else None
    return m["fstype"] == "devtmpfs" and n in harden.DEV_BIND_NODES and m["root"] == f"/{n}"


def overlay_options(lower: str, upper: str, work: str, userxattr: bool = False) -> str:
    """userxattr: the slot ran in its own user namespace, where overlayfs keeps its metadata (opaque dirs, redirects) in user.overlay.*
    xattrs instead of trusted.overlay.*; the restore root must read the same ones or the restored tree sees a different filesystem."""
    for what, path in (("lower", lower), ("upper", upper), ("work", work)):
        if _OVERLAY_BAD.search(path):
            raise ContractError(f"the {what} path {path!r} contains a comma, colon or space: it cannot be an overlay mount option")
    return f"lowerdir={lower},upperdir={upper},workdir={work}" + (",userxattr" if userxattr else "")


def restore_argv(marker: str, generation: int, vcpus: int, memory_mib: int, images: str, pidfile: str, log: str, lower: str, upper: str,
                 work: str, criu_root: str, file_locks: bool = False, action_script: str = None, userxattr: bool = False,
                 externals: tuple = ()) -> tuple:
    """criu restore inside the constrained scope AND a private mount namespace in which the overlay is mounted at `criu_root` (the --root).
    Every value reaches `sh` as a positional parameter, never inside the script text. externals: restore_externals() of the dumped
    mount table (device binds and view binds re-bound from the host)."""
    script = 'mount -t overlay overlay -o "$1" "$2" || exit 97; shift 2; exec "$@"'
    return (*L.scope_prefix(f"drumsbench-{marker}-r{generation}.scope", vcpus, memory_mib),
            "unshare", "--mount", "--propagation", "private", "--",
            "sh", "-c", script, "sh", overlay_options(lower, upper, work, userxattr), criu_root,
            "criu", "restore", "-D", images, "--root", criu_root, "--restore-detached", *(["--file-locks"] if file_locks else []), *(["--action-script", action_script] if action_script else []), *externals, *L.CRIU_COMMON, "--pidfile", pidfile, "-o", log)
