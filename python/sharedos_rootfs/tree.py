"""Slot init, stage A (runs as pid 1 of the slot's namespaces, on the HOST's Python): overlay root, private /dev, hostname, then
`pivot_root` into the overlay and exec of stage B, the supervisor, as the IMAGE's own python (see supervisor.py).

    unshare ... python -m sharedos_rootfs.tree --marker M --plan service-plan.json

Why pivot_root and an image-side supervisor (CRIU's rules, read at the pinned commit): criu/mount.c requires the dumped root task's
/proc/PID/root to be "/" of the mount namespace ("The root task has another root than mntns"), which a chroot inside the host's
mount namespace is not. After pivot_root the old root is detached, so the namespace's root IS the overlay and only the slot's
mounts remain. A process that pivoted while still running host Python would map host files (interpreter, libc) that are outside the
new root, so stage A execs the image's own python3 with the supervisor source as `-c` text (no file or library is injected into
the image) and the plan on stdin, after moving its stdio off host files. It never runs outside its own pid namespace. PlanError
means the inspection cannot support the requested profile and the slot is NOT started (fail closed).
"""

from __future__ import annotations

import argparse
import fcntl
import json
import os
import re
import signal
import socket
import stat
import struct
import subprocess
import sys
import time

# name -> (major, minor): the standard Linux character devices a container needs. Created natively on the slot's PRIVATE tmpfs
# /dev: bind-mounting single host device files made CRIU refuse the mount tree ("mnt:..:./dev/tty doesn't have a proper root
# mount": a bind of one file of the host's devtmpfs has a non-root fs root and the host devtmpfs is not part of the slot).
DEV_NODES = {"null": (1, 3), "zero": (1, 5), "full": (1, 7), "random": (1, 8), "urandom": (1, 9), "tty": (5, 0)}
DEV_LINKS = {"fd": "/proc/self/fd", "stdin": "/proc/self/fd/0", "stdout": "/proc/self/fd/1", "stderr": "/proc/self/fd/2"}
SECURE_PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
# The image's administrator groups (sudoers %sudo/%admin, polkit AdminIdentities, wheel) and the groups that give root through
# a daemon's socket (lxd, docker). The agent acts as the image user and gets no privilege escalation: the slot never applies
# these groups to the user's processes (arm.build_plan leaves them out) and its /etc/group and /etc/gshadow do not list the
# user in them (strip_admin_groups).
ADMIN_GROUPS = ("sudo", "admin", "wheel", "lxd", "docker")


def strip_member(text: str, groups, user: str, fields) -> str:
    """The text of /etc/group (members: field 3) or /etc/gshadow (administrators: field 2, members: field 3) with `user`
    removed from those fields of every group named in `groups`. Every other line and every other name is kept as it is."""
    out = []
    for line in text.split("\n"):
        f = line.split(":")
        if len(f) == 4 and f[0] in groups:
            for i in fields:
                f[i] = ",".join(m for m in f[i].split(",") if m != user)
            line = ":".join(f)
        out.append(line)
    return "\n".join(out)


class PlanError(Exception):
    pass


FUSE_SUPER_MAGIC = 0x65735546


def statfs_type(path: str) -> int:
    """f_type of statfs(path) (os has no statfs: libc through ctypes; struct statfs starts with the long f_type)."""
    import ctypes
    buf = ctypes.create_string_buffer(256)
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.statfs(path.encode(), buf) != 0:
        raise OSError(ctypes.get_errno(), f"statfs {path}")
    return ctypes.c_long.from_buffer(buf).value


def mount_steps(plan: dict) -> list:
    """Ordered ('run', argv) | ('mkdir', path) | ('touch', path) | ('symlink', target, path) steps."""
    m, lower = plan["merged"], plan["lower"]
    s = [("run", ["mount", "--make-rprivate", "/"]),
         ("mkdir", plan["upper"]), ("mkdir", plan["work"]), ("mkdir", m),
         ("run", ["mount", "-t", "overlay", "overlay", "-o",
                  f"lowerdir={lower},upperdir={plan['upper']},workdir={plan['work']}", m]),
         ("run", ["mount", "-t", "proc", "proc", f"{m}/proc"]),
         ("run", ["mount", "-t", "sysfs", "-o", "ro,nosuid,nodev,noexec", "sysfs", f"{m}/sys"]),
         ("run", ["mount", "-t", "tmpfs", "-o", "mode=755,size=64m", "tmpfs", f"{m}/dev"])]
    tty_gid = plan["tty_gid"]
    if plan.get("userns"):
        # In its own user namespace the slot's /dev tmpfs is SB_I_NODEV: a node made there can be neither created nor opened.
        # The standard nodes are bind mounts of the host's (as runc/podman do); CRIU gets them as external mounts (harden.py).
        for n in DEV_NODES:
            s += [("touch", f"{m}/dev/{n}"), ("run", ["mount", "--bind", f"/dev/{n}", f"{m}/dev/{n}"])]
    else:
        for n, (major, minor) in DEV_NODES.items():
            s.append(("mknod", f"{m}/dev/{n}", 0o666, major, minor, 0, tty_gid if n == "tty" else 0))
    for n, target in DEV_LINKS.items():
        s.append(("symlink", target, f"{m}/dev/{n}"))
    s += [("mkdir", f"{m}/dev/pts"),
          ("run", ["mount", "-t", "devpts", "-o", f"newinstance,ptmxmode=0666,mode=0620,gid={tty_gid}", "devpts", f"{m}/dev/pts"]),
          ("symlink", "pts/ptmx", f"{m}/dev/ptmx"),
          ("mkdir", f"{m}/dev/shm"), ("run", ["mount", "-t", "tmpfs", "-o", "mode=1777,size=512m", "tmpfs", f"{m}/dev/shm"]),
          ("run", ["mount", "-t", "tmpfs", "-o", "mode=1777,size=1g", "tmpfs", f"{m}/tmp"]),
          ("run", ["mount", "-t", "tmpfs", "-o", "mode=755,size=256m", "tmpfs", f"{m}/run"])]
    pv = plan.get("procview")
    if pv:
        # the slot's own memory/CPU view (drumsd procview), bound read-only over the kernel's files; the source must be the
        # live procview FUSE mount (checked by the op), never a regular host file
        for name, target in sorted(pv["files"].items()):
            s.append(("bind_view", f"{pv['dir'].rstrip('/')}/{name}", f"{m}{target}"))
    return s


SESSION_ENV_NAME = re.compile(r"^(XDG_[A-Z_]+|GNOME_[A-Z_]+|DESKTOP_SESSION)$")


def session_env(session: dict) -> dict:
    """The variables a display manager + gnome-session would export for the image's own session, derived ONLY from files in
    the image (inspection `session`): XDG_CURRENT_DESKTOP from the session file's DesktopNames, XDG_SESSION_DESKTOP /
    DESKTOP_SESSION from its name, GNOME_SHELL_SESSION_MODE and other literal variables from its `env` Exec prefix and the
    image's gdm env.d files. Nothing is invented; unsafe names (PATH, LD_*, HOME ...) are never taken."""
    env = {}
    for src in (session.get("gdm_env") or {}, session.get("exec_env") or {}):
        for k, v in src.items():
            if SESSION_ENV_NAME.match(k) and "$" not in v and "`" not in v:
                env[k] = v
    if session.get("desktop_names"):
        env["XDG_CURRENT_DESKTOP"] = session["desktop_names"]
    if session.get("name"):
        env["XDG_SESSION_DESKTOP"] = env["DESKTOP_SESSION"] = session["name"]
    env["XDG_SESSION_CLASS"] = "user"
    return env


# systemd's default PATH for its managers and units when /usr is not merged (systemd's DEFAULT_PATH_NORMAL), what pid 1 gives
# a system unit before the environment generators run
SYSTEMD_DEFAULT_PATH = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
# Drums' own addition, recorded as a difference: the slot has no GPU device, so Mesa is told to use its software rasterizer
# (llvmpipe) directly; the VMs have none either (no virgl) and end up in the same rasterizer.
DRUMS_ENV = {"LIBGL_ALWAYS_SOFTWARE": "1"}


def _login(plan: dict):
    return (plan.get("inspection") or {}).get("login")


def identity_env(plan: dict) -> dict:
    u = plan["user"]
    return {"HOME": u["home"], "USER": u["name"], "LOGNAME": u["name"], "SHELL": u.get("shell") or "/bin/bash"}


def user_env(plan: dict) -> dict:
    """The session's environment BEFORE the login steps the supervisor runs in the slot (keyring, generators, X session): what
    PAM and the display manager put in it. With the inspection's `login` facts: the image's pam_env files (/etc/environment
    and /etc/default/locale for gdm-autologin), the variables gdm sets (GDMSESSION, USERNAME: only when their names are in the
    image's gdm binaries) and pam_systemd's XDG_RUNTIME_DIR / session bus. Without them (an old inspection, or the
    display_only profile): the earlier fixed environment with LANG=C.UTF-8."""
    u = plan["user"]
    env = dict(identity_env(plan), PATH=SECURE_PATH, DISPLAY=plan["display"], XDG_RUNTIME_DIR=f"/run/user/{u['uid']}",
               DBUS_SESSION_BUS_ADDRESS=f"unix:path=/run/user/{u['uid']}/bus", XDG_SESSION_TYPE="x11", **DRUMS_ENV)
    sess = (plan.get("inspection") or {}).get("session") or {}
    env.update(session_env(sess))
    env.setdefault("GNOME_SHELL_SESSION_MODE", "ubuntu")
    login = _login(plan)
    if login and plan.get("profile") == "gnome_x11":
        if login.get("gdm_sets_gdmsession") and sess.get("name"):
            env["GDMSESSION"] = sess["name"]
        if login.get("gdm_sets_username"):
            env["USERNAME"] = u["name"]
        env.update(login.get("pam_env") or {})
        if not any(k in env for k in ("LANG", "LC_ALL")):
            env["LANG"] = "C.UTF-8"
    else:
        env["LANG"] = "C.UTF-8"
        env.update(plan.get("server", {}).get("env") or {})
    return env


def service_env(plan: dict) -> dict:
    """What systemd (pid 1) gives a system unit before its environment generators run: its default PATH and the image's
    locale (/etc/locale.conf, else /etc/default/locale). The generators, User= and the unit's Environment= come on top."""
    login = _login(plan) or {}
    return dict(PATH=SYSTEMD_DEFAULT_PATH, **(login.get("locale") or {"LANG": "C.UTF-8"}))


def server_env_extra(plan: dict) -> dict:
    """The guest server is the image's system unit: User= (identity) and its Environment= lines, plus Drums' own variables."""
    return dict(identity_env(plan), **(plan.get("server", {}).get("env") or {}), **DRUMS_ENV)


HOSTNAME = re.compile(r"^[A-Za-z0-9]([A-Za-z0-9.-]{0,62})$")


def _check_identity(plan: dict) -> None:
    """The image user's groups, the tty gid and the hostname must be sane before anything is mounted or dropped to."""
    u = plan["user"]
    for g in u.get("groups", []):
        if isinstance(g, bool) or not isinstance(g, int) or not (0 < g < 65534):
            raise PlanError(f"supplementary gid {g!r} is not acceptable (gid 0 or out of range is never applied)")
    bad = [n for n in u.get("group_names", []) if n in ADMIN_GROUPS]
    if bad:
        raise PlanError(f"the plan gives the desktop user the administrator group(s) {bad}: agents get no privilege escalation")
    if u["gid"] in u.get("groups", []):
        raise PlanError("the primary gid is listed as supplementary")
    tg = plan.get("tty_gid")
    if isinstance(tg, bool) or not isinstance(tg, int) or not (0 < tg < 65534):
        raise PlanError(f"the image has no usable `tty` group in /etc/group (gid {tg!r}): /dev/tty ownership cannot be exact")
    hn = plan.get("hostname")
    if hn is not None and not HOSTNAME.match(hn):
        raise PlanError(f"hostname {hn!r} from the image is not a valid host name")


def services_plan(plan: dict) -> list:
    """Specs {name, argv, cwd, wait, critical}. Raises PlanError when the inspection cannot support the profile."""
    insp = plan["inspection"]
    if not insp.get("ok_for_server"):
        raise PlanError(f"inspection is not ok_for_server: {insp.get('blockers')}")
    bins, uid = insp["binaries"], plan["user"]["uid"]
    _check_identity(plan)
    width_height = plan["screen"].rsplit("x", 1)[0]
    decision = insp["display"]["decision"]
    sock = f"/tmp/.X11-unix/X{plan['display'].lstrip(':')}"
    specs = []
    if plan["profile"] == "gnome_x11":
        # GNOME Shell talks to the SYSTEM bus (UPower, logind, ...) while it starts; with no system bus its JS fails at
        # ui/status/power.js. The image's own dbus-daemon provides it; nothing is stubbed or emulated.
        if "gnome-shell" not in bins:
            raise PlanError("profile gnome_x11 needs gnome-shell, which the image does not contain")
        sysb = insp.get("system_bus") or {}
        if not (sysb.get("system_conf") and sysb.get("messagebus_user")):
            raise PlanError(f"the image has no usable system bus configuration (needed by gnome_x11): {sysb}")
        sess = insp.get("session") or {}
        if not sess.get("desktop_names"):
            raise PlanError("the image has no verifiable GNOME session file (DesktopNames): GNOME Desktop Icons needs "
                            "XDG_CURRENT_DESKTOP and it will not be guessed")
        specs.append({"name": "system_dbus", "argv": [bins["dbus-daemon"], "--system", "--nofork", "--nopidfile"], "user": "root",
                      "mkdirs": {"/run/dbus": 0o755}, "wait": {"path": "/run/dbus/system_bus_socket"}, "critical": True})
    specs.append({"name": "dbus", "argv": [bins["dbus-daemon"], "--session", f"--address=unix:path=/run/user/{uid}/bus",
                                           "--nofork", "--nopidfile"], "wait": {"path": f"/run/user/{uid}/bus"}, "critical": True})
    if decision == "xvfb_in_image":
        specs.append({"name": "display", "argv": [bins["Xvfb"], plan["display"], "-screen", "0", plan["screen"],
                                                  "-nolisten", "tcp", "-noreset"], "wait": {"path": sock}, "critical": True})
    elif decision in ("xorg_dummy_in_image", "xorg_dummy_staged"):
        # the real server binary, not the setuid wrapper, so a non-root slot user can start it with a private config
        specs.append({"name": "display", "argv": [bins.get("Xorg_real") or bins["Xorg"], plan["display"], "-config",
                                                  "/tmp/xorg-dummy.conf", "-logfile", "/var/log/sharedos/Xorg.log", "-noreset",
                                                  "-nolisten", "tcp"], "wait": {"path": sock}, "critical": True,
                      "write": {"/tmp/xorg-dummy.conf": _dummy_conf(width_height)}})
    else:
        raise PlanError(f"display decision {decision!r}: the image has no headless X server and none was added")
    login = _login(plan) if plan["profile"] == "gnome_x11" else None
    if plan["profile"] == "gnome_x11":
        if "gnome-shell" not in bins:
            raise PlanError("profile gnome_x11 needs gnome-shell, which the image does not contain")
        if login is not None:
            if login.get("unsupported"):
                raise PlanError(f"the image's login cannot be reproduced: {login['unsupported']}")
            # the image's own login, run in the slot as the user once the display and the session bus are up (supervisor `login`)
            specs.append({"name": "login", "kind": "login", "base": "session_base", "out": "session",
                          "keyring": login.get("keyring"), "generators": login.get("user_generators") or [],
                          "pre_display": [exec_argv(x["exec"] or "") for x in login.get("pre_display") or []],
                          "ibus_im": bool(login.get("session_sets_ibus_im")),
                          "xsession": login.get("xsession"), "set": login.get("session_manager_env") or {},
                          "activation": login.get("dbus_update_activation_environment"), "timeout_s": 120, "critical": True})
        # the desktop IS the workload for gnome_x11: a dead GNOME is a failed slot, not a tolerated one
        specs.append({"name": "desktop", "argv": [bins["gnome-shell"], "--x11"], "wait": {"alive_s": 10}, "critical": True,
                      **({"env": "session"} if login is not None else {})})
        # gnome-session's RequiredComponents other than the shell (the GNOME settings daemons: XSETTINGS, media keys, keyboard
        # ...), started as gnome-session starts them, after the shell. Not critical: a daemon that needs a system service the
        # slot does not run (gsd-power needs systemd-logind) exits at start; the supervisor logs it ("NOT READY <name>") and
        # the slot runs without it, a recorded difference from the VM, never hidden.
        for c in (login or {}).get("components") or []:
            argv = exec_argv(c["exec"] or "")
            if not argv:
                raise PlanError(f"session component {c['id']} has no Exec in {c['file']}")
            specs.append({"name": c["id"].rsplit(".", 1)[-1].lower(), "argv": argv, "env": "session", "wait": {"alive_s": 1},
                          "critical": False, "component": c["id"]})
    elif plan["profile"] != "display_only":
        raise PlanError(f"unknown profile {plan['profile']!r}")
    srv = plan["server"]
    specs.append({"name": "server", "argv": srv["argv"], "cwd": srv["cwd"],
                  "wait": {"tcp": ["127.0.0.1", plan["guest_service_port"]], "timeout_s": 180}, "critical": True,
                  **({"env": "service", "env_extra": server_env_extra(plan)} if login is not None else {})})
    if login is not None:
        # first of all: the environment pid 1 would give system units (the image's system environment generators)
        specs.insert(0, {"name": "service-env", "kind": "env", "base": "service_base", "out": "service",
                         "generators": login.get("system_generators") or [], "timeout_s": 60, "critical": True})
        for sp in specs:
            if sp.get("user") == "root":
                sp.setdefault("env", "service")
    return specs


def exec_argv(line: str) -> list:
    """argv from a desktop-entry Exec line (field codes such as %U removed)."""
    import shlex
    return [t for t in shlex.split(line) if not re.fullmatch(r"%[a-zA-Z]", t)]


def payload_envs(plan: dict) -> dict:
    return {"session_base": user_env(plan), "service_base": service_env(plan)}


def _dummy_conf(res: str) -> str:
    w, h = res.split("x")
    return ('Section "Device"\n  Identifier "d0"\n  Driver "dummy"\n  VideoRam 256000\nEndSection\n'
            'Section "Monitor"\n  Identifier "m0"\n  HorizSync 5.0 - 1000.0\n  VertRefresh 5.0 - 200.0\nEndSection\n'
            f'Section "Screen"\n  Identifier "s0"\n  Device "d0"\n  Monitor "m0"\n  DefaultDepth 24\n'
            f'  SubSection "Display"\n    Depth 24\n    Modes "{w}x{h}"\n  EndSubSection\nEndSection\n')


def lo_up() -> None:
    """Bring up loopback in the current net namespace (SIOCSIFFLAGS) without needing `ip` in the image."""
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        ifr = struct.pack("16sH14x", b"lo", 0)
        flags = struct.unpack("16sH14x", fcntl.ioctl(s, 0x8913, ifr))[1]          # SIOCGIFFLAGS
        fcntl.ioctl(s, 0x8914, struct.pack("16sH14x", b"lo", flags | 0x1 | 0x40))  # IFF_UP | IFF_RUNNING
    finally:
        s.close()


def preload_host_modules() -> None:
    """Import, BEFORE the pivot, host modules stage A may need lazily afterwards (the pivot detaches the host filesystem)."""
    import ctypes  # noqa: F401
    import encodings.ascii  # noqa: F401
    import encodings.idna  # noqa: F401
    import encodings.latin_1  # noqa: F401
    import encodings.utf_8  # noqa: F401
    import platform  # noqa: F401
    import traceback  # noqa: F401
    try:
        socket.getaddrinfo("127.0.0.1", 0)
    except OSError:
        pass


PIVOT_ROOT_NR = {"x86_64": 155, "aarch64": 41}     # the syscall has no libc wrapper
MS_REC, MS_PRIVATE, MNT_DETACH = 0x4000, 1 << 18, 2


def pivot_root_into(new_root: str, libc=None, osmod=os) -> None:
    """Make `new_root` (a mount point) the root of this mount namespace and detach the old root with everything mounted under it.
    The `pivot_root(".", ".")` form (pivot_root(2) NOTES) needs no put_old directory, so nothing is created in the overlay."""
    import ctypes
    import platform
    libc = libc or ctypes.CDLL(None, use_errno=True)       # the already-loaded libc: nothing is opened from the host filesystem
    nr = PIVOT_ROOT_NR.get(platform.machine())
    if nr is None:
        raise OSError(f"pivot_root syscall number unknown for {platform.machine()}")

    def check(rc, what):
        if rc != 0:
            e = ctypes.get_errno()
            raise OSError(e, f"{what} failed: {os.strerror(e)}")
    old = osmod.open("/", osmod.O_RDONLY | osmod.O_DIRECTORY)
    new = osmod.open(new_root, osmod.O_RDONLY | osmod.O_DIRECTORY)
    try:
        osmod.fchdir(new)
        check(libc.syscall(nr, b".", b"."), "pivot_root")
        osmod.fchdir(old)                                   # the old root, now stacked on top of the new one
        check(libc.mount(None, b".", None, MS_REC | MS_PRIVATE, None), "make the old root private")
        check(libc.umount2(b".", MNT_DETACH), "detach the old root")
        osmod.chdir("/")
    finally:
        osmod.close(old)
        osmod.close(new)


class Ops:
    """The system calls stage A makes, injectable for tests."""
    def run(self, argv):
        subprocess.run(argv, check=True)

    def mkdir(self, p):
        os.makedirs(p, exist_ok=True)

    def bind_view(self, src, dst):
        """Read-only bind of one procview file over `dst`. Refuses a source that is not on the procview FUSE mount (fail closed:
        a slot never starts with the host's or a stale view)."""
        fs = statfs_type(os.path.dirname(src))
        if fs != FUSE_SUPER_MAGIC:
            raise PlanError(f"procview source {src} is not on a FUSE mount (f_type {fs:#x}): is drums-procview running?")
        with open(src):      # the view answers this process only if it is in the slot's cgroup: proves the slot will read it
            pass
        subprocess.run(["mount", "--bind", src, dst], check=True)
        subprocess.run(["mount", "-o", "remount,bind,ro,nosuid,nodev,noexec", dst], check=True)

    def touch(self, p):
        open(p, "a").close()

    def mknod(self, path, mode, major, minor, uid, gid):
        """A native character device node with exact mode and owner (mknod is subject to umask, so chmod/chown explicitly).
        Failure is an error: there is no fallback to a host bind."""
        if os.path.lexists(path):
            os.unlink(path)
        os.mknod(path, stat.S_IFCHR | mode, os.makedev(major, minor))
        os.chmod(path, mode)
        os.chown(path, uid, gid)

    def become_session_leader(self):
        """The launcher runs `setsid` BEFORE `unshare --pid --fork`, so the namespace init inherits a session and process group whose
        leader is OUTSIDE the pid namespace (seen from inside as sid=0 pgid=0). CRIU refuses that ("A session leader of ... is outside
        of its pid namespace"). pid 1 is not a group leader, so it can start its own session: from then on the init, and every
        service it spawns, belongs to a session whose leader is inside the namespace."""
        os.setsid()
        return os.getpid(), os.getsid(0), os.getpgid(0)

    def set_hostname(self, name):
        socket.sethostname(name)        # the slot's own UTS namespace: the host's hostname is untouched

    def symlink(self, target, p):
        if not os.path.lexists(p):
            os.symlink(target, p)

    def lo_up(self):
        lo_up()

    def prepare_dir(self, path, mode, uid=None, gid=None):
        os.makedirs(path, exist_ok=True)
        os.chmod(path, mode)
        if uid is not None:
            os.chown(path, uid, gid)

    def write_file(self, path, text):
        with open(path, "w") as f:
            f.write(text)

    def strip_admin_groups(self, merged, user, groups):
        """Take `user` out of the administrator groups in the slot's /etc/group and /etc/gshadow (through the overlay: the
        image's lower layer is never written). The files keep their mode and owner (rewritten in place). Fails closed: the
        slot does not start if the user is still listed."""
        for rel, fields in (("etc/group", (3,)), ("etc/gshadow", (2, 3))):
            path = os.path.join(merged, rel)
            try:
                with open(path) as f:
                    old = f.read()
            except FileNotFoundError:
                continue
            new = strip_member(old, groups, user, fields)
            if new != old:
                fd = os.open(path, os.O_WRONLY | os.O_TRUNC)
                with os.fdopen(fd, "w") as f:
                    f.write(new)
            with open(path) as f:
                left = [l.split(":")[0] for l in f.read().split("\n")
                        if len(l.split(":")) == 4 and l.split(":")[0] in groups and user in l.split(":")[3].split(",")]
            if left:
                raise PlanError(f"{rel} still lists {user} in {left}")

    def preload(self):
        preload_host_modules()

    def harden_runtime(self):
        """no_new_privs + capability bounding-set drop + seccomp, applied once in the init and inherited by every
        service the supervisor spawns. Runs after the session is owned and before the first mount."""
        from . import harden
        harden.apply_runtime_hardening()

    def apply_metadata_firewall(self):
        """Drop slot egress to the metadata endpoint and host-local ranges inside the slot's net namespace."""
        from . import harden
        subprocess.run(harden.metadata_nft_argv(), input=harden.metadata_nft_ruleset().encode(), check=True)

    def supervisor_source(self) -> str:
        with open(os.path.join(os.path.dirname(os.path.abspath(__file__)), "supervisor.py")) as f:
            return f.read()

    def pivot_root(self, new_root):
        pivot_root_into(new_root)

    def redirect_stdio(self, log_path):
        """stdout/stderr were opened by the launcher on a HOST file; after the pivot that file is unreachable (and CRIU refuses an fd
        it cannot reach), so both move to a log inside the slot's root. stdin is replaced by the plan fd at exec."""
        fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o644)
        os.dup2(fd, 1)
        os.dup2(fd, 2)
        os.close(fd)

    def exec_supervisor(self, python: str, source: str, payload: dict, marker: str):
        """Replace this process with the image's python running the supervisor. The plan goes in an anonymous memfd as stdin; every
        other inherited fd is closed. `-m sharedos_rootfs.tree --marker M` after the source are ignored by Python and keep the
        process identifiable by marker. Never returns."""
        fd = os.memfd_create("plan", 0)
        os.write(fd, json.dumps(payload).encode())
        os.lseek(fd, 0, os.SEEK_SET)
        os.dup2(fd, 0)
        os.closerange(3, 4096)
        os.execv(python, [python, "-c", source, "-m", "sharedos_rootfs.tree", "--marker", marker])


def install_term_handler() -> None:
    def on_term(signum, frame):
        raise SystemExit(0)   # stage A is stopping before the exec: nothing was started yet
    signal.signal(signal.SIGTERM, on_term)


def bring_up(plan: dict, ops: Ops, marker: str = "?", getpid=os.getpid) -> None:
    """Mounts, hostname, preparation INSIDE the new root's tree, pivot_root, stdio move, exec of the supervisor. In production
    exec_supervisor never returns; any failure before it raises with nothing started."""
    if getpid() != 1:
        raise PlanError("refusing to run: not pid 1 of a private pid namespace")
    specs = services_plan(plan)            # fail closed before any mount
    env = user_env(plan)
    pid, sid, pgid = ops.become_session_leader()      # before any mount or service: the init owns its own session
    if not (pid == sid == pgid):
        raise PlanError(f"the init is not the leader of its own session inside the pid namespace (pid={pid} sid={sid} pgid={pgid}): "
                        "CRIU would refuse to dump it")
    # The firewall first: nft is exec'd, and an exec after the bounding-set drop has no CAP_NET_ADMIN (it is not in the allowlist, so
    # nothing in the slot can ever change these rules); loaded here it is in place before any service exists.
    if plan.get("harden", True):           # absent = hardened; false only with the trusted-only opt-out (harden.enabled)
        ops.apply_metadata_firewall()      # block metadata/host-local egress in the slot's own net namespace
        ops.harden_runtime()               # no_new_privs + cap bounding-set drop + seccomp, before any mount
    src = ops.supervisor_source()          # read while the host filesystem is still the root
    for step in mount_steps(plan):
        getattr(ops, step[0])(*step[1:]) if step[0] != "run" else ops.run(step[1])
    ops.lo_up()
    if plan.get("hostname"):
        ops.set_hostname(plan["hostname"])       # the image's own /etc/hostname; its /etc/hosts is used as it is, never rewritten
    ops.preload()
    m, u = plan["merged"], plan["user"]
    # no privilege escalation for the agent: the image user is in no administrator group inside the slot
    ops.strip_admin_groups(m, u["name"], ADMIN_GROUPS)
    # everything the services need on disk is prepared through the overlay's host-visible path BEFORE the pivot
    ops.prepare_dir(f"{m}/run/user/{u['uid']}", 0o700, u["uid"], u["gid"])
    ops.prepare_dir(f"{m}/tmp/.X11-unix", 0o1777)
    # Owned by the image user: Xorg opens its own -logfile AFTER the privilege drop; the supervisor's per-service logs are opened by root.
    ops.prepare_dir(f"{m}/var/log/sharedos", 0o755, u["uid"], u["gid"])
    for spec in specs:
        for path, mode in (spec.get("mkdirs") or {}).items():
            ops.prepare_dir(m + path, mode)
        for path, text in (spec.get("write") or {}).items():
            ops.write_file(m + path, text)
    payload = {"specs": [{k: v for k, v in sp.items() if k not in ("mkdirs", "write")} for sp in specs], "env": env,
               "envs": payload_envs(plan),
               "user": {"uid": u["uid"], "gid": u["gid"], "groups": u.get("groups", [])}, "log_dir": "/var/log/sharedos"}
    python = plan["inspection"]["binaries"]["python3"]
    ops.pivot_root(m)                      # the namespace root IS the overlay now; the host filesystem is detached
    ops.redirect_stdio("/var/log/sharedos/tree.log")
    ops.exec_supervisor(python, src, payload, marker)


def main(argv=None, ops=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--marker", required=True)
    ap.add_argument("--plan", required=True)
    a = ap.parse_args(argv)
    plan = json.load(open(a.plan))
    install_term_handler()
    try:
        bring_up(plan, ops or Ops(), a.marker)
    except SystemExit:
        return 0
    except Exception as e:      # any failure: nothing has been started yet (the supervisor owns the services)
        print(f"slot failed: {type(e).__name__}: {e}", file=sys.stderr, flush=True)
        try:
            import traceback
            traceback.print_exc(file=sys.stderr)
        except Exception:
            pass
        return 5
    return 0    # only reachable with a test double: the real exec_supervisor replaces this process


if __name__ == "__main__":
    sys.exit(main())
