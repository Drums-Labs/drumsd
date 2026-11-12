"""Inspect a mounted (read-only) OSWorld root filesystem and decide what the slot runtime can use.

  python -m sharedos_rootfs.rootfs_inspect ROOT --out inspection.json --mount-state STATE.json

STATE.json is the file `python -m sharedos_rootfs.image attach` wrote; its verified image_sha256 is recorded in the
inspection, and the arm refuses an inspection that does not carry the pinned image hash.

Pure file reads. It answers: which user, which display stack, where is the guest server (and does its main.py
equal the pinned public source), how does the image itself start it, are the Python dependencies present, and
what would have to be ADDED (and therefore recorded as a difference from the image). Nothing is assumed:
anything not found is reported as missing and blocks `ok_for_server`.
"""

from __future__ import annotations

import glob
import hashlib
import json
import os
import re
import shlex
import sys

from . import PINNED_SERVER_MAIN_SHA256

BIN_DIRS = ("usr/bin", "bin", "usr/sbin", "sbin", "usr/local/bin", "usr/lib/xorg")
WANT_BINARIES = ("Xvfb", "Xorg", "dbus-daemon", "dbus-launch", "dbus-run-session", "nautilus", "gnome-shell",
                 "gnome-session", "python3", "python", "systemctl", "xdotool", "ip")
PY_ROOTS = ("usr/lib/python3/dist-packages", "usr/local/lib/python3*/dist-packages", "usr/local/lib/python3*/site-packages",
            "usr/lib/python3*/site-packages", "home/*/.local/lib/python3*/site-packages", "opt/*/lib/python3*/site-packages",
            "opt/*/*/lib/python3*/site-packages", "home/*/*/lib/python3*/site-packages", "home/*/*/*/lib/python3*/site-packages")
WANT_PY = {"pyautogui": "pyautogui", "flask": "flask", "Xlib": "Xlib", "lxml": "lxml", "PIL": "PIL", "requests": "requests",
           "pyatspi": "pyatspi"}
SERVER_SEARCH = ("home/*", "home/*/*", "home/*/*/*", "opt/*", "opt/*/*", "srv/*", "srv/*/*", "usr/local/*", "root", "root/*")
UNIT_DIRS = ("etc/systemd/system", "lib/systemd/system", "usr/lib/systemd/system", "etc/systemd/user",
             "usr/lib/systemd/user", "home/*/.config/systemd/user", "home/*/.config/autostart", "etc/xdg/autostart")


def _read(path, limit=2_000_000):
    try:
        with open(path, "rb") as f:
            return f.read(limit)
    except OSError:
        return None


def _sha(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def _exists(root, rel):
    return os.path.lexists(os.path.join(root, rel))


def os_release(root):
    raw = _read(os.path.join(root, "etc/os-release"))
    out = {}
    for line in (raw or b"").decode(errors="replace").splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            out[k] = v.strip().strip('"')
    return out


def users(root):
    raw = (_read(os.path.join(root, "etc/passwd")) or b"").decode(errors="replace")
    out = []
    for line in raw.splitlines():
        f = line.split(":")
        if len(f) >= 7 and f[2].isdigit() and f[3].isdigit() and int(f[2]) >= 1000 and f[0] != "nobody":
            out.append({"name": f[0], "uid": int(f[2]), "gid": int(f[3]), "home": f[5], "shell": f[6]})
    return out


def group_facts(root, user):
    """Facts from the IMAGE's /etc/group only (never the host's): the user's supplementary groups (named in a member list, other
    than the primary gid) and the gid of `tty` for the /dev/tty node and the devpts mount."""
    raw = (_read(os.path.join(root, "etc/group")) or b"").decode(errors="replace")
    sup, tty = [], None
    for line in raw.splitlines():
        f = line.split(":")
        if len(f) < 4 or not f[2].isdigit():
            continue
        if f[0] == "tty":
            tty = int(f[2])
        if user and user["name"] in [m for m in f[3].split(",") if m] and int(f[2]) != user["gid"]:
            sup.append({"name": f[0], "gid": int(f[2])})
    return {"user_supplementary": sup, "tty_gid": tty}


def hostname_facts(root):
    """The image's own hostname and whether its /etc/hosts maps it (sudo and others resolve the hostname through that file)."""
    name = (_read(os.path.join(root, "etc/hostname")) or b"").decode(errors="replace").strip() or None
    hosts = (_read(os.path.join(root, "etc/hosts")) or b"").decode(errors="replace")
    maps = bool(name) and any(name in line.split("#")[0].split()[1:] for line in hosts.splitlines() if line.strip())
    return {"etc_hostname": name, "etc_hosts_maps_it": maps, "etc_hosts_sha256": hashlib.sha256(hosts.encode()).hexdigest()}


def binaries(root):
    found = {}
    for name in WANT_BINARIES:
        for d in BIN_DIRS:
            if _exists(root, f"{d}/{name}"):
                found[name] = f"/{d}/{name}"
                break
    if _exists(root, "usr/lib/xorg/Xorg"):
        found["Xorg_real"] = "/usr/lib/xorg/Xorg"     # Ubuntu's /usr/bin/Xorg is a root-rights wrapper around this
    return found


def python_modules(root):
    found = {}
    for label, pkg in WANT_PY.items():
        for pat in PY_ROOTS:
            hits = glob.glob(os.path.join(root, pat, pkg)) + glob.glob(os.path.join(root, pat, pkg + "-*")) \
                + glob.glob(os.path.join(root, pat, pkg + ".py"))
            if hits:
                found[label] = "/" + os.path.relpath(hits[0], root)
                break
    return found


def find_servers(root):
    """main.py files that look like the OSWorld guest server (serve /screenshot and /execute)."""
    out, seen = [], set()
    for pat in SERVER_SEARCH:
        for d in glob.glob(os.path.join(root, pat)):
            p = os.path.join(d, "main.py")
            if p in seen or not os.path.isfile(p):
                continue
            seen.add(p)
            data = _read(p) or b""
            if (b"'/screenshot'" in data or b'"/screenshot"' in data) and (b"'/execute'" in data or b'"/execute"' in data):
                out.append({"path": "/" + os.path.relpath(p, root), "sha256": _sha(p), "bytes": len(data),
                            "matches_pinned_public_source": _sha(p) == PINNED_SERVER_MAIN_SHA256,
                            "siblings": sorted(os.listdir(d))[:60]})
    return out


def _kv_lines(text):
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if line and not line.startswith("#") and "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip().strip('"')
    return out


def session_info(root):
    """The image's own GNOME session definition, read from files only: the xsession .desktop (DesktopNames, the `env K=V` prefix
    of Exec, its basename) and the literal variables of the image's gdm env.d files. None if the image has no gnome-session."""
    best = None
    for f in sorted(glob.glob(os.path.join(root, "usr/share/xsessions/*.desktop"))):
        text = (_read(f) or b"").decode(errors="replace")
        entry = _kv_lines(text.split("[Desktop Entry]", 1)[-1])
        if "gnome-session" in entry.get("Exec", ""):
            cand = {"file": "/" + os.path.relpath(f, root), "name": os.path.splitext(os.path.basename(f))[0],
                    "desktop_names": entry.get("DesktopNames", "").replace(";", ":").strip(":") or None,
                    "exec": entry.get("Exec"), "exec_env": {}}
            toks = shlex.split(entry["Exec"])
            if toks and os.path.basename(toks[0]) == "env":
                for t in toks[1:]:
                    if "=" in t and not t.startswith("-"):
                        k, v = t.split("=", 1)
                        cand["exec_env"][k] = v
                    else:
                        break
            if best is None or cand["name"] == "ubuntu":
                best = cand
    if best is None:
        return None
    gdm = {}
    for f in sorted(glob.glob(os.path.join(root, "usr/share/gdm/env.d/*.env"))):
        gdm.update(_kv_lines((_read(f) or b"").decode(errors="replace")))
    return {**best, "gdm_env": gdm}


LOCALE_NAMES = re.compile(r"^(LANG|LANGUAGE|LC_[A-Z_]+)$")
ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
GENERATOR_DIRS = {"user": ("etc/systemd/user-environment-generators", "usr/local/lib/systemd/user-environment-generators",
                           "usr/lib/systemd/user-environment-generators"),
                  "system": ("etc/systemd/system-environment-generators", "usr/local/lib/systemd/system-environment-generators",
                             "usr/lib/systemd/system-environment-generators")}


def _env_file(text):
    """KEY=VALUE lines of an environment file (pam_env's /etc/environment and envfile=, /etc/default/locale): no expansion,
    one pair of surrounding quotes removed, comments and malformed lines skipped."""
    out = {}
    for line in text.splitlines():
        line = line.strip()
        if line.startswith("export "):
            line = line[7:].strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        k, v = k.strip(), v.strip()
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
            v = v[1:-1]
        if ENV_NAME.match(k):
            out[k] = v
    return out


def _generators(root, kind):
    """systemd environment generators in systemd's search order: a name in an earlier directory masks the same name later;
    run in the lexical order of their names (systemd.environment-generator(7))."""
    seen = {}
    for d in GENERATOR_DIRS[kind]:
        for f in sorted(glob.glob(os.path.join(root, d, "*"))):
            n = os.path.basename(f)
            if n not in seen:
                seen[n] = f
    out = []
    for n in sorted(seen):
        f = seen[n]
        if os.path.isfile(f) and os.access(f, os.X_OK) and os.path.getsize(f) > 0:
            out.append("/" + os.path.relpath(f, root))
    return out


def _pam_lines(root, service):
    """The session lines of an /etc/pam.d service, with @include expanded (one level per file, cycles ignored)."""
    out, seen = [], set()

    def walk(name):
        if name in seen:
            return
        seen.add(name)
        for line in (_read(os.path.join(root, "etc/pam.d", name)) or b"").decode(errors="replace").splitlines():
            line = line.split("#", 1)[0].strip()
            if line.startswith("@include"):
                walk(line.split()[1])
            elif line.startswith("session"):
                out.append(line)
    walk(service)
    return out


def _strings(path, names):
    """Which of `names` occur as NUL-terminated literals in a binary (the values a program sets for itself)."""
    data = _read(path, 50_000_000) or b""
    return {n for n in names if (b"\0" + n.encode() + b"\0") in data}


def login_info(root, session, user):
    """What an autologin through the image's own display manager gives the session, read from the image's files only.

    The image logs `user` in with gdm3 (gdm-autologin PAM service) and runs the X session through /etc/gdm3/Xsession, under a
    systemd user manager whose environment generators run first. The guest server is a SYSTEM unit, so its environment is the
    one systemd (pid 1) gives every system service: the system generators, the locale, User= and the unit's Environment=.
    Recorded here: the PAM env files (pam_env readenv=1 -> /etc/environment, envfile=), whether pam_gnome_keyring starts the
    keyring at login, the generators, the X session script, the components of the session (RequiredComponents of the
    gnome-session file and the Exec of their autostart entries) and the variables gnome-session sets for itself (only those
    whose literal names are in the image's gnome-session binary). Anything the runtime cannot reproduce is in `unsupported`."""
    if not session:
        return None
    pam = _pam_lines(root, "gdm-autologin")
    unsupported = []
    env_files = []
    for line in pam:
        w = line.split()
        if len(w) >= 3 and w[2] == "pam_env.so":
            args = dict(a.split("=", 1) if "=" in a else (a, "1") for a in w[3:])
            if args.get("readenv", "1") == "1":
                env_files.append(args.get("envfile", "/etc/environment"))
            if args.get("user_readenv") == "1" and user and _exists(root, f"{user['home'].lstrip('/')}/.pam_environment"):
                unsupported.append("~/.pam_environment (pam_env user_readenv=1) is present")
    conf = [l for l in (_read(os.path.join(root, "etc/security/pam_env.conf")) or b"").decode(errors="replace").splitlines()
            if l.strip() and not l.strip().startswith("#")]
    if conf:
        unsupported.append(f"/etc/security/pam_env.conf has {len(conf)} entries (its syntax is not reproduced)")
    pam_env = {}
    for f in dict.fromkeys(env_files):
        pam_env.update(_env_file((_read(os.path.join(root, f.lstrip("/"))) or b"").decode(errors="replace")))
    # systemd (pid 1) on Debian/Ubuntu: /etc/locale.conf, else /etc/default/locale
    loc_file = next((f for f in ("/etc/locale.conf", "/etc/default/locale") if _exists(root, f.lstrip("/"))), None)
    locale = {k: v for k, v in _env_file((_read(os.path.join(root, loc_file.lstrip("/"))) or b"").decode(errors="replace")).items()
              if LOCALE_NAMES.match(k)} if loc_file else {}
    keyring = any(l.split()[2:3] == ["pam_gnome_keyring.so"] and "auto_start" in l.split() for l in pam)
    sname = None
    for t in shlex.split(session.get("exec") or ""):
        if t.startswith("--session="):
            sname = t.split("=", 1)[1]
    components, missing = [], []
    sfile = f"usr/share/gnome-session/sessions/{sname}.session" if sname else None
    req = _kv_lines((_read(os.path.join(root, sfile)) or b"").decode(errors="replace")).get("RequiredComponents", "") if sfile else ""
    for cid in [c for c in req.split(";") if c]:
        if cid == "org.gnome.Shell":
            continue                                    # the desktop service itself
        f = next((d for d in (f"etc/xdg/autostart/{cid}.desktop", f"usr/share/gnome/autostart/{cid}.desktop",
                              f"usr/share/applications/{cid}.desktop") if _exists(root, d)), None)
        if not f:
            missing.append(cid)
            continue
        e = _kv_lines((_read(os.path.join(root, f)) or b"").decode(errors="replace").split("[Desktop Entry]", 1)[-1])
        components.append({"id": cid, "file": "/" + f, "exec": e.get("Exec"), "phase": e.get("X-GNOME-Autostart-Phase")})
    if missing:
        unsupported.append(f"session components without an autostart entry: {missing}")
    gs = "usr/libexec/gnome-session-binary"
    gs_vars = _strings(os.path.join(root, gs), ("XDG_MENU_PREFIX", "GNOME_DESKTOP_SESSION_ID", "gnome-", "this-is-deprecated"))
    session_manager_env = {}
    if {"XDG_MENU_PREFIX", "gnome-"} <= gs_vars:
        session_manager_env["XDG_MENU_PREFIX"] = "gnome-"
    if {"GNOME_DESKTOP_SESSION_ID", "this-is-deprecated"} <= gs_vars:
        session_manager_env["GNOME_DESKTOP_SESSION_ID"] = "this-is-deprecated"
    # gnome-session sets the IBus variables for X clients when the session's input method is IBus (GTK_IM_MODULE=ibus from
    # im-config): QT_IM_MODULE=ibus and XMODIFIERS=@im=ibus, measured in the VM's session; applied only when those literals
    # are in the image's gnome-session binary (inferred mapping, checked against the VM by the session environment test)
    im_ibus = _strings(os.path.join(root, gs), ("QT_IM_MODULE", "XMODIFIERS", "@im=ibus")) == {"QT_IM_MODULE", "XMODIFIERS", "@im=ibus"}
    gdm_vars = sorted(_strings(os.path.join(root, "usr/libexec/gdm-session-worker"), ("USERNAME",))
                      | _strings(os.path.join(root, "usr/libexec/gdm-x-session"), ("USERNAME",))
                      | _strings(os.path.join(root, "usr/sbin/gdm3"), ("GDMSESSION",)))
    # gnome-session runs its PreDisplayServer autostart entries first and takes the variables they print into the session
    # (the gnome-keyring components print SSH_AUTH_SOCK ...); entries for other desktops or whose condition is false are
    # skipped as gnome-session skips them (OnlyShowIn/NotShowIn, AutostartCondition if-exists/unless-exists under ~/.config)
    desktops = set((session.get("desktop_names") or "").split(":"))
    pre_display = []
    for f in sorted(glob.glob(os.path.join(root, "etc/xdg/autostart/*.desktop"))):
        e = _kv_lines((_read(f) or b"").decode(errors="replace").split("[Desktop Entry]", 1)[-1])
        if e.get("X-GNOME-Autostart-Phase") != "PreDisplayServer" or e.get("Hidden") == "true":
            continue
        only = set(filter(None, e.get("OnlyShowIn", "").split(";")))
        if (only and not only & desktops) or set(filter(None, e.get("NotShowIn", "").split(";"))) & desktops:
            continue
        cond = e.get("AutostartCondition", "").split()
        if cond:
            if cond[0] not in ("if-exists", "unless-exists") or len(cond) != 2 or not user:
                unsupported.append(f"{os.path.basename(f)}: AutostartCondition {' '.join(cond)!r} is not reproduced")
                continue
            there = _exists(root, f"{user['home'].lstrip('/')}/.config/{cond[1]}")
            if there != (cond[0] == "if-exists"):
                continue
        pre_display.append({"file": "/" + os.path.relpath(f, root), "exec": e.get("Exec")})
    xsession = next((f for f in ("/etc/gdm3/Xsession", "/etc/gdm/Xsession") if _exists(root, f.lstrip("/"))), None)
    if not xsession:
        unsupported.append("no gdm Xsession script")
    duae = "/usr/bin/dbus-update-activation-environment" if _exists(root, "usr/bin/dbus-update-activation-environment") else None
    return {"pam_service": "gdm-autologin", "pam_env_files": list(dict.fromkeys(env_files)), "pam_env": pam_env,
            "locale_file": loc_file, "locale": locale,
            "keyring": "/usr/bin/gnome-keyring-daemon" if keyring and _exists(root, "usr/bin/gnome-keyring-daemon") else None,
            "user_generators": _generators(root, "user"), "system_generators": _generators(root, "system"),
            "xsession": xsession, "session_name": sname, "session_file": "/" + sfile if sfile else None,
            "components": components, "pre_display": pre_display, "session_manager_env": session_manager_env, "gdm_sets_username": "USERNAME" in gdm_vars,
            "gdm_sets_gdmsession": "GDMSESSION" in gdm_vars, "session_sets_ibus_im": im_ibus,
            "dbus_update_activation_environment": duae, "unsupported": unsupported}


def find_launchers(root, server_path):
    """Units / autostart entries that start the server, with their Exec lines (how the IMAGE starts it)."""
    out = []
    name = os.path.basename(os.path.dirname(server_path)) or "server"
    for pat in UNIT_DIRS:
        for d in glob.glob(os.path.join(root, pat)):
            for f in sorted(glob.glob(os.path.join(d, "*"))):
                if not os.path.isfile(f):
                    continue
                text = (_read(f, 200_000) or b"").decode(errors="replace")
                if server_path in text or re.search(r"(^|/)%s/main\.py" % re.escape(name), text):
                    execs = [l.split("=", 1)[1].strip() for l in text.splitlines()
                             if l.startswith(("ExecStart=", "Exec="))]
                    users_ = [l.split("=", 1)[1].strip() for l in text.splitlines() if l.startswith("User=")]
                    wd = [l.split("=", 1)[1].strip() for l in text.splitlines() if l.startswith("WorkingDirectory=")]
                    envs = [l.split("=", 1)[1].strip() for l in text.splitlines() if l.startswith("Environment=")]
                    out.append({"file": "/" + os.path.relpath(f, root), "exec": execs, "user": users_,
                                "working_directory": wd, "environment": envs})
    return out


def exec_argv(line: str) -> list:
    """argv from a systemd/desktop Exec line (strips the @ - + ! prefixes and %-specifiers)."""
    line = re.sub(r"%[a-zA-Z%]", "", line.strip())
    line = line.lstrip("@-+!:")
    return shlex.split(line)


def inspect_rootfs(root: str, image_sha256=None) -> dict:
    """image_sha256: the sha256 `image attach` verified for the qcow2 that `root` is mounted from (None = unknown)."""
    root = os.path.abspath(root)
    rel = os_release(root)
    us = users(root)
    bins = binaries(root)
    mods = python_modules(root)
    servers = find_servers(root)
    server = servers[0] if len(servers) == 1 else None
    launchers = find_launchers(root, server["path"]) if server else []
    dummy = _exists(root, "usr/lib/xorg/modules/drivers/dummy_drv.so")
    if "Xvfb" in bins:
        display = "xvfb_in_image"
    elif "Xorg" in bins and dummy:
        display = "xorg_dummy_in_image"
    else:
        display = "needs_added_display_package"
    user = next((u for u in us if u["name"] == "user"), None)
    passwd = (_read(os.path.join(root, "etc/passwd")) or b"").decode(errors="replace")
    system_bus = {"system_conf": _exists(root, "usr/share/dbus-1/system.conf") or _exists(root, "etc/dbus-1/system.conf"),
                  "messagebus_user": any(l.split(":")[0] == "messagebus" for l in passwd.splitlines()),
                  "machine_id": _exists(root, "var/lib/dbus/machine-id") or _exists(root, "etc/machine-id")}
    blockers = []
    if not rel:
        blockers.append("/etc/os-release missing: this does not look like a root filesystem")
    if user is None:
        blockers.append(f"no 'user' account with uid>=1000 (found {[u['name'] for u in us]})")
    if not servers:
        blockers.append("no OSWorld guest server main.py found (a main.py serving /screenshot and /execute)")
    elif len(servers) > 1:
        blockers.append(f"ambiguous: {len(servers)} candidate server main.py files {[s['path'] for s in servers]}")
    if "python3" not in bins:
        blockers.append("python3 not found in the image")
    for m in ("pyautogui", "flask", "Xlib"):
        if m not in mods:
            blockers.append(f"python module {m} not found in the image")
    if "dbus-daemon" not in bins:
        blockers.append("dbus-daemon not found in the image")
    return {
        "root": root, "image_sha256": image_sha256, "os_release": {k: rel.get(k) for k in ("ID", "VERSION_ID", "PRETTY_NAME", "VERSION_CODENAME")},
        "kernel_modules_dirs": sorted(os.listdir(os.path.join(root, "lib/modules"))) if os.path.isdir(os.path.join(root, "lib/modules")) else [],
        "users": us, "user": user, "binaries": bins, "python_modules": mods, "server_candidates": servers,
        "server": server, "launchers": launchers, "system_bus": system_bus, "session": session_info(root),
        "login": login_info(root, session_info(root), user),
        "groups": group_facts(root, user), "hostname": hostname_facts(root),
        "autologin": _autologin(root), "desktop_dir": bool(user and _exists(root, f"{user['home'].lstrip('/')}/Desktop")),
        "display": {"decision": display, "xorg_dummy_driver": dummy,
                    "added_packages_needed": ["xvfb"] if display == "needs_added_display_package" else [],
                    "note": "added packages go in the upper layer and are a recorded difference from the exact image"},
        "gnome_shell_present": "gnome-shell" in bins, "systemd_present": "systemctl" in bins,
        "server_matches_pinned_public_source": bool(server and server["matches_pinned_public_source"]),
        "blockers": blockers, "ok_for_server": not blockers,
    }


def _autologin(root):
    raw = (_read(os.path.join(root, "etc/gdm3/custom.conf")) or _read(os.path.join(root, "etc/gdm/custom.conf")) or b"").decode(errors="replace")
    return {"enabled": bool(re.search(r"^\s*AutomaticLoginEnable\s*=\s*true", raw, re.M | re.I)),
            "user": (re.search(r"^\s*AutomaticLogin\s*=\s*(\S+)", raw, re.M) or [None, None])[1]}


def _mount_state_sha(path):
    with open(path) as f:
        st = json.load(f)
    sha = st.get("image_sha256")
    if not (isinstance(sha, str) and re.fullmatch(r"[0-9a-f]{64}", sha)):
        raise SystemExit(f"{path}: no image_sha256 (use the state file written by `python -m sharedos_rootfs.image attach`)")
    return sha


if __name__ == "__main__":
    a = sys.argv[1:]
    if len(a) not in (3, 5) or a[1] != "--out" or (len(a) == 5 and a[3] != "--mount-state"):
        sys.exit(__doc__)
    rep = inspect_rootfs(a[0], _mount_state_sha(a[4]) if len(a) == 5 else None)
    with open(a[2], "x") as f:
        json.dump(rep, f, indent=2)
    print(json.dumps({"ok_for_server": rep["ok_for_server"], "blockers": rep["blockers"], "image_sha256": rep["image_sha256"],
                      "display": rep["display"]["decision"], "server": (rep["server"] or {}).get("path"),
                      "server_sha256": (rep["server"] or {}).get("sha256"),
                      "server_matches_pinned_public_source": rep["server_matches_pinned_public_source"]}, indent=2))
    sys.exit(0 if rep["ok_for_server"] else 1)
