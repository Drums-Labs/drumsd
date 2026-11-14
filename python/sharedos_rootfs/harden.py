"""Slot hardening: user namespace mapping, capability bounding-set drop, no_new_privs, a default-allow
seccomp blocklist, and a per-slot metadata/host-local network firewall.

Threat model. A Drums slot runs untrusted, model-generated actions as the OSWorld guest user, and the guest's
sudo password is public, so the agent can become root INSIDE the slot at will. Before this module the slot init
ran as real host uid 0 with the full host capability set and no seccomp (see launch.py history): slot root WAS
host root, so a single init_module()/kexec()/mount of a host block device was full host compromise by design.

The controls here, in order of how much they matter:
  1. USER NAMESPACE (the load-bearing one). The slot runs in its own user namespace with slot uids 0..65535
     mapped to an unprivileged host range (SUBID_BASE..). Slot root is host uid SUBID_BASE, which owns nothing
     on the host. Capabilities the slot init holds are capabilities in ITS userns only; they are powerless over
     host-owned inodes, devices and kernel state. Module loading already fails in a non-initial userns because
     init_module() needs CAP_SYS_MODULE in the INITIAL userns, which a nested userns never has.
  2. no_new_privs + seccomp (defense in depth). Even a userns root should not be able to reach kernel attack
     surface that has no place in a desktop: module loading, kexec, bpf, perf, the kernel keyring, and
     open_by_handle_at (a classic namespace escape). A default-ALLOW filter returns EPERM for that blocklist and
     lets everything else through, so GNOME, Xorg, Chrome, VS Code (Electron) and LibreOffice run unchanged.
  3. CAPABILITY BOUNDING SET dropped to a desktop-minimal allowlist, so no child can regain a dangerous cap via
     file capabilities, while the init keeps its effective caps for the mounts it performs during bring-up.
  4. METADATA / HOST-LOCAL FIREWALL. The slot's private net namespace is given nft rules that drop egress to the
     cloud metadata endpoint (169.254.169.254, link-local, ULA) and to host-local services, so a slot cannot
     read instance credentials or reach the host loopback.

Chrome's own sandbox needs user namespaces and seccomp ITSELF: it calls unshare(CLONE_NEWUSER|...) and installs
its own seccomp-bpf filter under no_new_privs. The blocklist below deliberately leaves unshare, clone, clone3,
seccomp and prctl allowed so Chrome's nested sandbox still builds; our filter is merely the outer floor it nests
under. CRIU restores the nested user namespaces via the fork's nested-userns support (--nested-pidns on both
sides); nothing here adds a flag CRIU does not already carry.

Pure functions (no side effects) are unit-testable on any OS. apply_runtime_hardening() and the nft application
touch the system and run only inside the slot init on Linux.
"""

from __future__ import annotations

import errno
import os

# ---- user namespace mapping --------------------------------------------------------------------------------

# Base of the unprivileged host sub-uid/sub-gid range a slot maps onto. Each slot gets a disjoint 65536-wide
# block (SUBID_BASE + slot*SUBID_COUNT) so no two slots share a host uid and one slot's files are never owned by
# another slot's mapped root. 100000 is above every system/human uid; 65536 covers the full in-slot uid space so
# GNOME's system accounts (messagebus, rtkit, colord, ...) all map.
#
# LIMIT (measured 2026-10-30, drums-pageshare): the per-slot range holds only for a slot that is PROVISIONED (launch_argv). A slot that
# is restored (branch, capture, reset) gets the user namespace of the checkpoint it is restored from, with the map in its images, so
# every slot branched from the origin (slot 0) runs as host uids SUBID_BASE.. like the origin. Restoring into another range would need
# CRIU to write a different map for the root user namespace AND every host-side id of the checkpoint (the upper layer's owners, the
# tmpfs contents CRIU saved) shifted by the same amount; neither exists yet. Siblings are still separated by their own pid, mount,
# net, ipc and uts namespaces and their own upper layers.
SUBID_BASE = 100000
SUBID_COUNT = 65536


def enabled(cfg: dict) -> bool:
    """The hardened profile is the default. `"harden": false` in the runtime config is the documented opt-out for TRUSTED-ONLY
    hosts (your own scripted policies, no untrusted model actions): slots then run as before this module, in pid/mount/net/
    ipc/uts namespaces as host root, with no user namespace, no_new_privs, capability drop, seccomp or metadata firewall, so
    root in a slot is root on the host. Anything but an explicit false keeps the hardening on."""
    return cfg.get("harden", True) is not False


def subid_base(slot: int) -> int:
    if slot < 0:
        raise ValueError(f"slot must be non-negative, got {slot}")
    return SUBID_BASE + slot * SUBID_COUNT


def uid_map_arg(slot: int) -> str:
    """unshare --map-users value: <outer-host-uid>,<inner-uid>,<count>. Maps inner 0..65535 to host base.."""
    return f"{subid_base(slot)},0,{SUBID_COUNT}"


def gid_map_arg(slot: int) -> str:
    return f"{subid_base(slot)},0,{SUBID_COUNT}"


# INSTALL REQUIREMENTS this module adds to the host (verified live on Ubuntu 24.04 / util-linux 2.39.3, kernel
# 7.0.0-1014-aws, 2026-10-30). The release install.sh/lock.json must provide all three or bring-up fails closed:
#   1. apt package `uidmap` (newuidmap/newgidmap) + `python3-seccomp` (the libseccomp binding, loaded inside the
#      slot image) + `nftables`. util-linux's --map-users shells out to newuidmap.
#   2. /etc/subuid and /etc/subgid must delegate the sub-id range to the launching user (root), e.g.
#      "root:100000:6553600" (100 slots x 65536). newuidmap enforces this delegation even for root: without it
#      the map is refused ("uid range ... not allowed"). See subid_delegation_lines().
#   3. python3-seccomp must exist INSIDE the OSWorld image (the slot init runs the image's python), not only on
#      the host. If it is absent, build_seccomp_filter() raises and the slot refuses to start (fail closed).


def subid_delegation_lines(user: str = "root", max_slots: int = 100) -> tuple:
    """The /etc/subuid and /etc/subgid line that authorizes `user` to map the per-slot host ranges. Must be
    written by the installer before the first slot launch; newuidmap refuses a map outside this delegation."""
    line = f"{user}:{SUBID_BASE}:{SUBID_COUNT * max_slots}"
    return (line, line)


def userns_unshare_flags(slot: int) -> tuple:
    """Flags added to the existing `unshare` line to put the slot in its own user namespace with slot root mapped
    to an unprivileged host uid, running as root (uid/gid 0) INSIDE the namespace. --map-users/--map-groups need
    util-linux >= 2.38 (present on the c8i image's Ubuntu) plus the `uidmap` package and the /etc/subuid /etc/subgid
    delegation above; the launcher is host-root but newuidmap still enforces that delegation."""
    return (
        "--user",
        "--map-users", uid_map_arg(slot),
        "--map-groups", gid_map_arg(slot),
        "--setuid", "0",
        "--setgid", "0",
    )


# ---- capabilities ------------------------------------------------------------------------------------------

# The bounding set the slot keeps. These are the caps a GNOME desktop + Xorg + the overlay/dev bring-up legitimately
# use INSIDE the userns (where they are namespaced and harmless to the host). Everything not listed is dropped from
# the bounding set, so no child can acquire it via file capabilities. Notably absent: CAP_SYS_MODULE, CAP_SYS_RAWIO,
# CAP_SYS_BOOT (kexec), CAP_MAC_ADMIN, CAP_SYS_TIME.
CAP_ALLOWLIST = (
    "CAP_CHOWN",
    "CAP_DAC_OVERRIDE",
    "CAP_DAC_READ_SEARCH",
    "CAP_FOWNER",
    "CAP_FSETID",
    "CAP_KILL",
    "CAP_SETGID",
    "CAP_SETUID",
    "CAP_SETPCAP",
    "CAP_NET_BIND_SERVICE",
    "CAP_NET_RAW",           # ping / Chrome net service
    "CAP_SYS_ADMIN",         # mounts, setns, pivot_root during bring-up (namespaced by the userns)
    "CAP_SYS_CHROOT",
    "CAP_SYS_PTRACE",        # in-slot debuggers, Chrome/Electron crash handler (userns-scoped)
    "CAP_SYS_NICE",
    "CAP_MKNOD",             # private /dev nodes
    "CAP_AUDIT_WRITE",
    "CAP_IPC_LOCK",
)

# Linux capability numbers (include/uapi/linux/capability.h); used to compute the PR_CAPBSET_DROP set.
_CAP_NUM = {
    "CAP_CHOWN": 0, "CAP_DAC_OVERRIDE": 1, "CAP_DAC_READ_SEARCH": 2, "CAP_FOWNER": 3, "CAP_FSETID": 4,
    "CAP_KILL": 5, "CAP_SETGID": 6, "CAP_SETUID": 7, "CAP_SETPCAP": 8, "CAP_LINUX_IMMUTABLE": 9,
    "CAP_NET_BIND_SERVICE": 10, "CAP_NET_BROADCAST": 11, "CAP_NET_ADMIN": 12, "CAP_NET_RAW": 13,
    "CAP_IPC_LOCK": 14, "CAP_IPC_OWNER": 15, "CAP_SYS_MODULE": 16, "CAP_SYS_RAWIO": 17, "CAP_SYS_CHROOT": 18,
    "CAP_SYS_PTRACE": 19, "CAP_SYS_PACCT": 20, "CAP_SYS_ADMIN": 21, "CAP_SYS_BOOT": 22, "CAP_SYS_NICE": 23,
    "CAP_SYS_RESOURCE": 24, "CAP_SYS_TIME": 25, "CAP_SYS_TTY_CONFIG": 26, "CAP_MKNOD": 27, "CAP_LEASE": 28,
    "CAP_AUDIT_WRITE": 29, "CAP_AUDIT_CONTROL": 30, "CAP_SETFCAP": 31, "CAP_MAC_OVERRIDE": 32,
    "CAP_MAC_ADMIN": 33, "CAP_SYSLOG": 34, "CAP_WAKE_ALARM": 35, "CAP_BLOCK_SUSPEND": 36, "CAP_AUDIT_READ": 37,
    "CAP_PERFMON": 38, "CAP_BPF": 39, "CAP_CHECKPOINT_RESTORE": 40,
}
CAP_LAST_CAP = 40


def caps_to_drop() -> tuple:
    """Capability numbers to remove from the bounding set: every known cap not in the allowlist."""
    keep = {_CAP_NUM[c] for c in CAP_ALLOWLIST}
    return tuple(n for n in range(CAP_LAST_CAP + 1) if n not in keep)


# ---- seccomp -----------------------------------------------------------------------------------------------

# Default action is ALLOW; each of these returns EPERM. Rationale: no GNOME/Xorg/Chrome/VS Code/LibreOffice path
# calls them, and each is host-dangerous kernel attack surface. Chrome's own sandbox (unshare/clone/clone3/
# seccomp/prctl) is intentionally NOT here, so its nested sandbox still builds under our floor. mount/umount are
# NOT blocked: the tree's bring-up and CRIU restore call them, and the userns already neuters host mounts.
SECCOMP_BLOCKED_SYSCALLS = (
    "init_module",
    "finit_module",
    "delete_module",
    "kexec_load",
    "kexec_file_load",
    "bpf",
    "perf_event_open",
    "keyctl",
    "add_key",
    "request_key",
    "open_by_handle_at",     # file-handle namespace escape
    "name_to_handle_at",
    "acct",
    "quotactl",
    "_sysctl",
    "nfsservctl",
    "swapon",
    "swapoff",
)

SECCOMP_ERRNO = errno.EPERM


def build_seccomp_filter(libseccomp=None):
    """Build a default-ALLOW seccomp filter that returns EPERM for SECCOMP_BLOCKED_SYSCALLS. Uses libseccomp
    (python3-seccomp) so syscall numbers resolve correctly on x86_64 and aarch64. Returns the SyscallFilter
    (not yet loaded); apply_runtime_hardening() loads it. Raises if libseccomp is unavailable so bring-up fails
    closed rather than running a slot with no seccomp."""
    if libseccomp is None:
        import seccomp as libseccomp  # python3-seccomp; required in the image
    flt = libseccomp.SyscallFilter(defaction=libseccomp.ALLOW)
    for name in SECCOMP_BLOCKED_SYSCALLS:
        try:
            flt.add_rule(libseccomp.ERRNO(SECCOMP_ERRNO), name)
        except (ValueError, RuntimeError):
            # syscall absent on this arch/kernel: nothing to block, skip (fail-open only for a nonexistent call)
            continue
    return flt


# ---- metadata / host-local egress firewall -----------------------------------------------------------------

# Addresses a slot must never reach. 169.254.169.254 is the cloud metadata endpoint (instance credentials); the
# rest is link-local and the host loopback / ULA that host-local services bind to. The host-side proxy still
# reaches the slot's server port by setns into the slot (proxy.py); this only restricts slot-initiated egress.
METADATA_BLOCK_V4 = ("169.254.0.0/16",)        # covers 169.254.169.254 and link-local
METADATA_BLOCK_V6 = ("fe80::/10", "fd00::/8")
HOST_LOCAL_BLOCK_V4 = ("127.0.0.0/8",)         # the slot has its own lo; host loopback is a different netns anyway


def metadata_nft_ruleset() -> str:
    """An nft ruleset (applied with `nft -f -` inside the slot's net namespace) that drops egress to the metadata
    endpoint and host-local ranges while leaving all other traffic, so the slot still reaches the internet through
    its NAT but cannot read instance credentials or reach host-local services."""
    v4 = ", ".join(METADATA_BLOCK_V4 + HOST_LOCAL_BLOCK_V4)
    v6 = ", ".join(METADATA_BLOCK_V6)
    return (
        "table inet drums_slot {\n"
        "  chain out {\n"
        "    type filter hook output priority 0; policy accept;\n"
        "    oifname \"lo\" accept;\n"
        f"    ip daddr {{ {v4} }} drop;\n"
        f"    ip6 daddr {{ {v6} }} drop;\n"
        "  }\n"
        "}\n"
    )


def metadata_nft_argv() -> tuple:
    return ("nft", "-f", "-")


# ---- runtime application (Linux, inside the slot init) ------------------------------------------------------

PR_SET_NO_NEW_PRIVS = 38
PR_CAPBSET_DROP = 24


def set_no_new_privs(prctl=None) -> None:
    """PR_SET_NO_NEW_PRIVS: no execve can ever gain privileges again. Required for an unprivileged seccomp load and
    inherited by every child, so GNOME/Chrome/VS Code all run under it."""
    if prctl is None:
        import ctypes
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        prctl = lambda *a: libc.prctl(*a)
    if prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
        raise OSError(ctypes_errno(), "prctl(PR_SET_NO_NEW_PRIVS) failed")


def drop_capability_bounding_set(prctl=None) -> None:
    """Remove every non-allowlisted cap from the bounding set so no child can regain it via file capabilities. The
    init keeps its EFFECTIVE caps (needed for the bring-up mounts); only the bounding set shrinks."""
    if prctl is None:
        import ctypes
        libc = ctypes.CDLL("libc.so.6", use_errno=True)
        prctl = lambda *a: libc.prctl(*a)
    for cap in caps_to_drop():
        rc = prctl(PR_CAPBSET_DROP, cap, 0, 0, 0)
        if rc != 0:
            # EINVAL for a cap number the running kernel does not know is benign; anything else is fatal.
            import ctypes
            e = ctypes.get_errno()
            if e != errno.EINVAL:
                raise OSError(e, f"prctl(PR_CAPBSET_DROP, {cap}) failed")


def ctypes_errno() -> int:
    import ctypes
    return ctypes.get_errno()


def apply_runtime_hardening(libseccomp=None, prctl=None) -> None:
    """Called by the slot init (tree.bring_up) after it becomes a session leader and before it execs the
    supervisor. Order matters: no_new_privs first (seccomp load needs it under an unprivileged euid), then drop the
    bounding set, then load the seccomp filter last so the filter itself governs nothing we still need to set up.
    Every child the supervisor spawns inherits all three."""
    set_no_new_privs(prctl=prctl)
    drop_capability_bounding_set(prctl=prctl)
    flt = build_seccomp_filter(libseccomp=libseccomp)
    flt.load()


# ---- host-side preparation for a slot in its own user namespace ---------------------------------------------
#
# Found in the app-compat regression of this change (2026-10-30, drums-pageshare): with slot root mapped to host uid B the slot
# could not start at all, for four reasons that every rootless/userns container runtime has to handle the same way:
#   1. The slot init (host uid B) cannot read its plan or reach its own upper/work/merged dirs: the state root and the slot dir are
#      0700 root. Docker's userns-remap uses the same scheme as here: the parent dirs are traversable (0711), the slot dir is
#      0710 root:<B> (only the slot's own mapped root group can enter it), its plan 0640 root:<B>, and upper/work/merged belong to B:B.
#   2. Every file of the read-only image is owned by host uid 0..: inside the user namespace they would all be "nobody" (unmapped),
#      so root in the slot could not write /etc and the image user could not write its own home. The lower layer is therefore an
#      IDMAPPED mount (mount_setattr(MOUNT_ATTR_IDMAP), kernel >= 5.12, ext4/overlayfs support it) that shows on-disk uid u as
#      host uid B+u; nothing is written to the image and nothing is chowned.
#   3. A filesystem mounted inside a user namespace is SB_I_NODEV: the native device nodes the tree used to mknod on its private
#      /dev tmpfs cannot be made (EPERM) or opened there. The six standard character devices are bind mounts of the host's nodes
#      (exactly what runc and podman do), declared to CRIU as external mounts on dump and on restore (CRIU_EXTERNAL_*).
#   4. Files the harness writes into the upper layer from the host (the staged display driver) must get the slot's ids too.

DEV_BIND_NODES = ("null", "zero", "full", "random", "urandom", "tty")
OPEN_TREE_CLONE, OPEN_TREE_CLOEXEC, AT_RECURSIVE = 1, 0o2000000, 0x8000
MOVE_MOUNT_F_EMPTY_PATH = 0x4
MOUNT_ATTR_IDMAP = 0x00100000
_SYS = {"x86_64": {"open_tree": 428, "move_mount": 429, "mount_setattr": 442},
        "aarch64": {"open_tree": 428, "move_mount": 429, "mount_setattr": 442}}
IDMAP_ROOT = "/run/drums-idmap"


def criu_dev_key(name: str) -> str:
    return f"drums-dev-{name}"


def criu_external_dump_args() -> tuple:
    """criu dump: the device binds are external mounts named by key (criu(8) --external mnt[MOUNTPOINT]:KEY)."""
    return tuple(x for n in DEV_BIND_NODES for x in ("--external", f"mnt[/dev/{n}]:{criu_dev_key(n)}"))


def criu_external_restore_args() -> tuple:
    """criu restore: each key is bound again from the host's own node (criu(8) --external mnt[KEY]:PATH)."""
    return tuple(x for n in DEV_BIND_NODES for x in ("--external", f"mnt[{criu_dev_key(n)}]:/dev/{n}"))


def idmapped_lower_path(lower: str, base: int) -> str:
    import hashlib
    h = hashlib.sha256(os.path.realpath(lower).encode()).hexdigest()[:8]
    return os.path.join(IDMAP_ROOT, f"{h}-{base}")


def _mountinfo_entry(path: str, mountinfo: str = "/proc/self/mountinfo"):
    want = os.path.realpath(path)
    found = None
    with open(mountinfo) as f:
        for line in f:
            pre = line.split(" - ")[0].split()
            if pre[4] == want:
                found = pre          # the last one is the top of the stack
    return found


def _userns_fd_for(base: int, count: int = SUBID_COUNT) -> int:
    """An fd of a user namespace whose map is `0 base count` (uid and gid), to idmap a mount with. Made by a child that unshares
    CLONE_NEWUSER and waits; the parent (host root) writes the maps, opens the namespace and lets the child exit."""
    r, w = os.pipe()
    pid = os.fork()
    if pid == 0:
        try:
            os.close(w)
            os.unshare(os.CLONE_NEWUSER)
            os.read(r, 1)            # wait for the maps and the open
        finally:
            os._exit(0)
    os.close(r)
    try:
        import time
        for _ in range(200):          # the child has unshared when its user ns differs from ours
            if os.readlink(f"/proc/{pid}/ns/user") != os.readlink("/proc/self/ns/user"):
                break
            time.sleep(0.005)
        else:
            raise OSError("the helper never entered its user namespace")
        with open(f"/proc/{pid}/uid_map", "w") as f:
            f.write(f"0 {base} {count}\n")
        with open(f"/proc/{pid}/setgroups", "w") as f:
            f.write("deny\n")
        with open(f"/proc/{pid}/gid_map", "w") as f:
            f.write(f"0 {base} {count}\n")
        return os.open(f"/proc/{pid}/ns/user", os.O_RDONLY | os.O_CLOEXEC)
    finally:
        os.close(w)
        os.waitpid(pid, 0)


def ensure_idmapped_lower(lower: str, base: int) -> str:
    """Host root: an idmapped, read-only view of `lower` in which on-disk id u reads as host id base+u (so as u inside the slot).
    Idempotent: an existing view at the path is reused only if mountinfo says it is idmapped. Returns its path."""
    import ctypes
    import platform
    dest = idmapped_lower_path(lower, base)
    cur = _mountinfo_entry(dest) if os.path.isdir(dest) else None
    if cur is not None and cur[4] == os.path.realpath(dest):
        if "idmapped" not in cur[5].split(","):
            raise OSError(f"{dest} is mounted but not idmapped: refusing to use it as a slot's lower layer")
        return dest
    os.makedirs(dest, mode=0o755, exist_ok=True)
    nr = _SYS[platform.machine()]
    libc = ctypes.CDLL(None, use_errno=True)
    libc.syscall.restype = ctypes.c_long

    def chk(rc, what):
        if rc < 0:
            e = ctypes.get_errno()
            raise OSError(e, f"{what}: {os.strerror(e)}")
        return rc
    usern = _userns_fd_for(base)
    try:
        tfd = chk(libc.syscall(nr["open_tree"], -100, os.fsencode(lower), OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC), f"open_tree({lower})")
        try:
            # struct mount_attr { u64 attr_set, attr_clr, propagation, userns_fd }
            attr = (ctypes.c_uint64 * 4)(MOUNT_ATTR_IDMAP | 0x1, 0, 0, usern)     # 0x1 = MOUNT_ATTR_RDONLY
            chk(libc.syscall(nr["mount_setattr"], tfd, b"", 0x1000, ctypes.byref(attr), ctypes.sizeof(attr)),   # AT_EMPTY_PATH
                "mount_setattr(MOUNT_ATTR_IDMAP)")
            chk(libc.syscall(nr["move_mount"], tfd, b"", -100, os.fsencode(dest), MOVE_MOUNT_F_EMPTY_PATH), f"move_mount({dest})")
        finally:
            os.close(tfd)
    finally:
        os.close(usern)
    cur = _mountinfo_entry(dest)
    if cur is None or "idmapped" not in cur[5].split(","):
        raise OSError(f"{dest} did not come up as an idmapped mount: {cur}")
    return dest


def prepare_slot_access(state_root: str, slot_dir: str, plan_file: str, owned_dirs, base: int) -> None:
    """Host root, before the launch: the minimum access the slot's mapped root needs (see 1. above), nothing more."""
    os.chmod(state_root, 0o711)
    os.chown(slot_dir, 0, base)
    os.chmod(slot_dir, 0o710)
    os.chown(plan_file, 0, base)
    os.chmod(plan_file, 0o640)
    for d in owned_dirs:
        os.chown(d, base, base)


def shift_owner(root: str, base: int, stop: str = None) -> list:
    """Host root: give every entry under `root` (a tree the harness wrote into an upper layer as host root) the slot's ids, keeping
    each entry's in-image owner: host id u becomes base+u. Entries already in the slot's range are left alone. Returns the paths."""
    done = []
    for d, dirs, files in os.walk(root):
        for n in [None] + dirs + files:
            p = d if n is None else os.path.join(d, n)
            st = os.lstat(p)
            if st.st_uid < base or st.st_gid < base:
                os.lchown(p, st.st_uid + base if st.st_uid < base else st.st_uid, st.st_gid + base if st.st_gid < base else st.st_gid)
                done.append(p)
    return done


class HostUserns:
    """The host-root operations above, as one injectable object (the arm's tests replace it with a recording fake)."""

    def idmapped_lower(self, lower: str, base: int) -> str:
        return ensure_idmapped_lower(lower, base)

    def shift_owner(self, root: str, base: int) -> list:
        return shift_owner(root, base)

    def prepare_access(self, state_root: str, slot_dir: str, plan_file: str, owned_dirs, base: int) -> None:
        prepare_slot_access(state_root, slot_dir, plan_file, owned_dirs, base)
