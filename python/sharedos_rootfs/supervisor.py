# drumsbench-sharedos-supervisor
"""Stage B of the slot init: runs INSIDE the slot's root, as the IMAGE's own Python, after the tree (stage A) has pivoted into
the overlay and executed `python3 -c <this file>`. It is therefore stdlib-only, Python 3.8-compatible, and imports nothing from
the sharedos_rootfs package or the host: every file mapped by this process (the interpreter, libc, stdlib) belongs to the image.
That is what lets CRIU dump the init: a dumped task's mapped files and fds must be reachable through the namespace's own root
(a host Python that chrooted into the overlay maps host files that are outside it). No host binary or library is copied into the
image; this script's source is passed on the command line, its input (the service plan) on stdin, nothing is written to the image.

    python3 -c <source> -m sharedos_rootfs.tree --marker M      (the `-m ... --marker M` words after the source are ignored by
                                                                 Python; they keep the process identifiable by marker)
The plan is one JSON object: {"specs": [...], "env": {...}, "user": {uid, gid, groups}, "log_dir": "/var/log/sharedos",
"prepare": [{path, mode, uid, gid}] (optional), "require_own_session": bool (optional)}. It arrives on stdin (custom runtime, stage A) or as
`--payload-json <json>` (OCI reference: the container process has no stdin plan).
Exit: 0 after SIGTERM and an orderly shutdown; 7 when a critical service died; 5 for any other failure.
"""

import ctypes
import json
import os
import re
import signal
import socket
import subprocess
import sys
import time

IPV4 = re.compile(r"^\d{1,3}(\.\d{1,3}){3}$")


PR_SET_NO_NEW_PRIVS = 38
_LIBC = ctypes.CDLL(None, use_errno=True)    # loaded here, not in a forked child


def no_new_privs():
    """prctl(PR_SET_NO_NEW_PRIVS, 1): from here on, exec never grants privileges (setuid sudo, su, pkexec and file
    capabilities do nothing), for this process and everything it starts. Called after the drop to the image user, in the
    child, before exec. The agent acts through these processes: it gets no privilege escalation."""
    if _LIBC.prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0:
        e = ctypes.get_errno()
        raise OSError(e, "prctl(PR_SET_NO_NEW_PRIVS): %s" % os.strerror(e))


class CriticalServiceDied(RuntimeError):
    """A service marked critical exited after startup: the slot is no longer a valid desktop."""


def tcp_probe(host, port, timeout=1.0):
    """One numeric AF_INET connect: no resolver, no text codec lookups."""
    if not IPV4.match(host):
        raise ValueError("tcp readiness host must be a dotted-quad literal, got %r" % (host,))
    s = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    try:
        s.settimeout(timeout)
        s.connect((host.encode("ascii"), int(port)))
        return True
    except OSError:
        return False
    finally:
        s.close()


class Ops:
    """The system calls the supervisor makes, injectable for tests. `log_dir` holds the per-service logs and the event log."""

    def __init__(self, log_dir="/var/log/sharedos"):
        self.log_dir = log_dir

    def prepare_dir(self, path, mode, uid=None, gid=None):
        os.makedirs(path, exist_ok=True)
        os.chmod(path, mode)
        if uid is not None:
            os.chown(path, uid, gid)

    def write_file(self, path, text):
        with open(path, "w") as f:
            f.write(text)

    def wait_for(self, w, proc):
        deadline = time.monotonic() + w.get("timeout_s", 60)
        if "alive_s" in w:
            time.sleep(w["alive_s"])
            if proc.poll() is not None:
                raise RuntimeError("exited rc=%s within %ss" % (proc.returncode, w["alive_s"]))
            return
        while time.monotonic() < deadline:
            if proc.poll() is not None:
                raise RuntimeError("exited rc=%s before becoming ready" % proc.returncode)
            if "path" in w and os.path.exists(w["path"]):
                return
            if "tcp" in w and tcp_probe(*w["tcp"]):
                return
            time.sleep(0.3)
        raise RuntimeError("not ready within %ss: %s" % (w.get("timeout_s", 60), w))

    def reap_orphans(self, known_pids):
        """Reap exited children that are NOT services (orphans reparented to pid 1), leaving service children for Popen.poll()."""
        reaped = []
        for tid in os.listdir("/proc/self/task"):
            try:
                with open("/proc/self/task/%s/children" % tid) as f:
                    kids = f.read().split()
            except OSError:
                continue
            for k in map(int, kids):
                if k in known_pids:
                    continue
                try:
                    done, _ = os.waitpid(k, os.WNOHANG)
                except ChildProcessError:
                    continue
                if done:
                    reaped.append(done)
        return reaped

    def spawn(self, spec, env, uid, gid, groups=()):
        os.makedirs(self.log_dir, exist_ok=True)
        log = open(os.path.join(self.log_dir, "%s.log" % spec["name"]), "ab")

        def drop():
            os.setgroups([gid] + list(groups))   # the image user's own supplementary groups; the host group database is never used
            os.setgid(gid)
            os.setuid(uid)
            no_new_privs()
        as_root = spec.get("user") == "root"
        run_env = dict(env, HOME="/root", USER="root", LOGNAME="root") if as_root else env
        try:
            return subprocess.Popen(spec["argv"], cwd=spec.get("cwd") or run_env["HOME"], env=run_env, stdout=log, stderr=log,
                                    stdin=subprocess.DEVNULL, preexec_fn=None if as_root else drop)
        except OSError as e:     # a missing binary or cwd is a named failure, not a bare traceback
            raise RuntimeError("cannot start %s %s: %s" % (spec["name"], spec["argv"][:1], e))
        finally:
            log.close()

    def capture(self, name, argv, env, uid, gid, groups=(), timeout=60, stdin=b""):
        """Run a login step to completion as the image user (stderr to the step's log). Returns (rc, stdout bytes)."""
        os.makedirs(self.log_dir, exist_ok=True)
        log = open(os.path.join(self.log_dir, "%s.log" % name), "ab")

        def drop():
            os.setgroups([gid] + list(groups))
            os.setgid(gid)
            os.setuid(uid)
            no_new_privs()
        try:
            p = subprocess.Popen(argv, cwd=env.get("HOME") or "/", env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                 stderr=log, preexec_fn=drop)
            out, _ = p.communicate(stdin, timeout=timeout)
            return p.returncode, out
        except subprocess.TimeoutExpired:
            p.kill()
            p.communicate()
            raise RuntimeError("%s: %s did not finish within %ss" % (name, argv[0], timeout))
        except OSError as e:
            raise RuntimeError("%s: cannot run %s: %s" % (name, argv[0], e))
        finally:
            log.close()

    def write_user_exec(self, path, text, uid, gid):
        fd = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o700)
        with os.fdopen(fd, "w") as f:
            f.write(text)
        os.chown(path, uid, gid)

    def remove(self, path):
        try:
            os.unlink(path)
        except OSError:
            pass

    def read_and_remove(self, path):
        try:
            with open(path, "rb") as f:
                return f.read()
        finally:
            try:
                os.unlink(path)
            except OSError:
                pass

    def write_json(self, name, obj):
        with open(os.path.join(self.log_dir, name), "w") as f:
            json.dump(obj, f, indent=1, sort_keys=True)

    def event(self, msg):
        line = "%s %s" % (time.strftime("%H:%M:%S"), msg)
        print(line, file=sys.stderr, flush=True)
        try:
            with open(os.path.join(self.log_dir, "tree-events.log"), "a") as f:
                f.write(line + "\n")
        except OSError:
            pass

    def tail(self, name, n=600):
        try:
            with open(os.path.join(self.log_dir, "%s.log" % name), "rb") as f:
                f.seek(0, 2)
                f.seek(max(0, f.tell() - n))
                return f.read().decode(errors="replace").strip()
        except OSError:
            return ""


def shutdown(procs, grace_s=5.0):
    live = [p for p in procs if p.poll() is None]
    for p in reversed(live):
        try:
            p.terminate()
        except ProcessLookupError:
            pass
    deadline = time.monotonic() + grace_s
    while time.monotonic() < deadline and any(p.poll() is None for p in live):
        time.sleep(0.05)
    killed = []
    for p in live:
        if p.poll() is None:
            killed.append(p.pid)
            p.kill()
    for p in live:
        p.wait()
    return {"sigterm_exits": [p.pid for p in live if p.pid not in killed], "sigkilled": killed}


def install_term_handler():
    def on_term(signum, frame):
        raise SystemExit(0)   # unwinds to main(), which shuts the services down
    signal.signal(signal.SIGTERM, on_term)


ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")


def parse_env_lines(raw):
    """KEY=VALUE lines (systemd environment generators, gnome-keyring-daemon --daemonize): one pair of quotes removed."""
    out = {}
    for line in raw.decode(errors="replace").splitlines():
        line = line.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        k, v = line.split("=", 1)
        if len(v) >= 2 and v[0] == v[-1] and v[0] in "\"'":
            v = v[1:-1]
        if ENV_NAME.match(k):
            out[k] = v
    return out


def parse_env0(raw):
    out = {}
    for item in raw.split(b"\0"):
        if b"=" in item:
            k, v = item.split(b"=", 1)
            k = k.decode(errors="replace")
            if ENV_NAME.match(k):
                out[k] = v.decode(errors="replace")
    return out


def run_generators(spec, env, ops, u, record):
    """systemd.environment-generator(7): each generator runs with the environment so far and prints assignments that update it;
    a failing generator is logged and skipped, as systemd does."""
    for g in spec.get("generators") or []:
        rc, out = ops.capture(spec["name"], [g], env, u["uid"], u["gid"], u.get("groups", ()), spec.get("timeout_s", 60))
        got = parse_env_lines(out) if rc == 0 else {}
        record.append({"step": "generator", "argv": [g], "rc": rc, "set": got})
        env.update(got)
    return env


def run_env_step(spec, envs, ops, u):
    """kind=env: the environment pid 1 gives system units (its base plus the system environment generators)."""
    record = []
    env = run_generators(spec, dict(envs[spec["base"]]), ops, u, record)
    envs[spec["out"]] = env
    ops.write_json("%s.json" % spec["name"], {"steps": record, "env": env})
    return env


def run_login(spec, envs, ops, u):
    """kind=login: the image's own autologin, in the order the display manager runs it, as the user, inside the slot:
    1. pam_gnome_keyring auto_start: `gnome-keyring-daemon --daemonize --login` (no password on autologin); the variables it
       prints go into the session (SSH_AUTH_SOCK ...);
    2. the systemd user manager's environment generators (environment.d, flatpak);
    3. the display manager's X session script (/etc/gdm3/Xsession: /etc/profile, ~/.profile, Xresources, Xsession.d), whose
       session command is replaced by `env -0` into a file, so the result is exactly the environment the script would hand
       to gnome-session;
    4. the variables gnome-session sets for itself (`set`, and the IBus variables), its PreDisplayServer autostart entries
       (whose printed variables join the session), then the whole environment is given to the session bus for activated
       services (dbus-update-activation-environment --all), as gnome-session does.
    The result is the environment of every session service (the shell, the settings daemons). Any failure fails the slot."""
    env = dict(envs[spec["base"]])
    record = []
    groups = u.get("groups", ())
    if spec.get("keyring"):
        argv = [spec["keyring"], "--daemonize", "--login"]
        rc, out = ops.capture(spec["name"], argv, env, u["uid"], u["gid"], groups, spec.get("timeout_s", 60))
        got = parse_env_lines(out)
        record.append({"step": "keyring", "argv": argv, "rc": rc, "set": got})
        if rc != 0:
            raise RuntimeError("login: %s exited rc=%s" % (argv[0], rc))
        env.update(got)
    run_generators(spec, env, ops, u, record)
    if spec.get("xsession"):
        # The session command must be one word: Xsession.d's 99x11-common_start runs `exec $STARTUP` (no eval, so no
        # redirection), with $STARTUP = the command wrapped by the fragments (im-launch). The command is a two-line script in
        # the user's runtime dir (the slot's tmpfs, not the image) that writes its environment next to itself.
        cmd = "%s/.drums-login-capture" % (env.get("XDG_RUNTIME_DIR") or "/tmp")
        ops.write_user_exec(cmd, '#!/bin/sh\nexec /usr/bin/env -0 > "$0.env"\n', u["uid"], u["gid"])
        argv = [spec["xsession"], cmd]
        try:
            rc, _ = ops.capture(spec["name"], argv, env, u["uid"], u["gid"], groups, spec.get("timeout_s", 60))
            raw = ops.read_and_remove(cmd + ".env")
        finally:
            ops.remove(cmd)
        got = parse_env0(raw)
        record.append({"step": "xsession", "argv": argv, "rc": rc, "vars": len(got)})
        if rc != 0 or not got:
            raise RuntimeError("login: the X session script %s exited rc=%s with %d variables" % (spec["xsession"], rc, len(got)))
        if "SSH_AGENT_PID" in got:
            raise RuntimeError("login: the X session script started ssh-agent around the session command (SSH_AUTH_SOCK was "
                               "not set by the keyring step); that agent is gone once the command exits, refusing its socket")
        got.pop("_", None)
        env = got
    env.update(spec.get("set") or {})
    if spec.get("ibus_im") and env.get("GTK_IM_MODULE") == "ibus":
        env.setdefault("QT_IM_MODULE", "ibus")
        env.setdefault("XMODIFIERS", "@im=ibus")
    for argv in spec.get("pre_display") or []:
        # gnome-session's PreDisplayServer autostart entries (the gnome-keyring components): run to completion, their
        # KEY=VALUE output goes into the session
        rc, out = ops.capture(spec["name"], argv, env, u["uid"], u["gid"], groups, spec.get("timeout_s", 60))
        got = parse_env_lines(out) if rc == 0 else {}
        record.append({"step": "pre_display", "argv": argv, "rc": rc, "set": got})
        env.update(got)
    if spec.get("activation"):
        argv = [spec["activation"], "--all"]
        rc, _ = ops.capture(spec["name"], argv, env, u["uid"], u["gid"], groups, spec.get("timeout_s", 60))
        record.append({"step": "activation", "argv": argv, "rc": rc})
        if rc != 0:
            raise RuntimeError("login: %s exited rc=%s" % (argv[0], rc))
    envs[spec["out"]] = env
    ops.write_json("%s.json" % spec["name"], {"steps": record, "env": env})
    return env


def start_services(payload, ops):
    """Start the services in order from the plan. Any failure (any exception) stops everything already started first.
    A spec with `kind` env/login is a step run to completion that makes a named environment; a service spec names the
    environment it runs with in `env` (default: the plan's `env`) plus its own `env_extra`."""
    u, env = payload["user"], payload["env"]
    envs = dict(payload.get("envs") or {})
    started = []
    try:
        for d in payload.get("prepare") or []:       # directories the services need (OCI mode: the runtime did the mounts, we do this)
            ops.prepare_dir(d["path"], d["mode"], d.get("uid"), d.get("gid"))
        for spec in payload["specs"]:
            if spec.get("kind") in ("env", "login"):
                try:
                    out = (run_env_step if spec["kind"] == "env" else run_login)(spec, envs, ops, u)
                except RuntimeError as e:
                    ops.event("FAILED %s: %s" % (spec["name"], e))
                    raise RuntimeError("step %s failed: %s" % (spec["name"], e))
                ops.event("done %s (%d variables)" % (spec["name"], len(out)))
                continue
            for path, mode in (spec.get("mkdirs") or {}).items():
                ops.prepare_dir(path, mode)
            for path, text in (spec.get("write") or {}).items():
                ops.write_file(path, text)
            if spec.get("env") and spec["env"] not in envs:
                raise RuntimeError("service %s needs the environment %r, which no earlier step made" % (spec["name"], spec["env"]))
            senv = dict(envs[spec["env"]] if spec.get("env") else env, **(spec.get("env_extra") or {}))
            p = ops.spawn(spec, senv, u["uid"], u["gid"], u.get("groups", ()))
            p.service, p.critical = spec["name"], bool(spec.get("critical"))
            started.append(p)
            ops.event("started %s pid=%s critical=%s argv0=%s" % (spec["name"], p.pid, p.critical, spec["argv"][0]))
            try:
                ops.wait_for(spec["wait"], p)
                ops.event("ready %s" % spec["name"])
            except RuntimeError as e:
                tail = ops.tail(spec["name"])
                ops.event("NOT READY %s: %s" % (spec["name"], e) + (" | log tail: %s" % tail[-300:] if tail else ""))
                if spec.get("critical"):
                    raise RuntimeError("service %s failed: %s" % (spec["name"], e) + (" | log tail: %s" % tail[-300:] if tail else ""))
        return started
    except BaseException:
        shutdown(started)
        raise


def supervise(procs, ops, sleep=time.sleep, ticks=None, interval_s=0.5):
    """Hold the slot alive only while every critical service is alive. Raises CriticalServiceDied the moment one has exited."""
    logged = set()
    n = 0
    while ticks is None or n < ticks:
        n += 1
        for p in procs:
            rc = p.poll()
            if rc is None:
                continue
            if p.critical:
                raise CriticalServiceDied("critical service %s exited rc=%s" % (p.service, rc))
            if p.pid not in logged:
                logged.add(p.pid)
                print("non-critical service %s exited rc=%s" % (p.service, rc), file=sys.stderr, flush=True)
        ops.reap_orphans({p.pid for p in procs})
        sleep(interval_s)


def main(argv=None, stdin=None, ops=None):
    argv = list(sys.argv[1:] if argv is None else argv)
    marker = argv[argv.index("--marker") + 1] if "--marker" in argv else "?"
    install_term_handler()
    procs = []
    try:
        if "--payload-json" in argv:        # OCI mode: a container process has no stdin plan, the plan is an argument
            raw = argv[argv.index("--payload-json") + 1]
        else:
            raw = (stdin or sys.stdin).read()
            if stdin is None:    # the plan arrived on an anonymous fd: drop it, fd 0 becomes the image's own /dev/null
                os.dup2(os.open(os.devnull, os.O_RDONLY), 0)
        payload = json.loads(raw)
        if payload.get("require_own_session") and not (os.getpid() == os.getsid(0) == os.getpgid(0)):
            # a maintained runtime (runc's nsexec calls setsid) must already have made the init a session leader
            raise RuntimeError("the init is not the leader of its own session: CRIU would refuse to dump it (pid=%d sid=%d pgid=%d)"
                               % (os.getpid(), os.getsid(0), os.getpgid(0)))
        ops = ops or Ops(payload.get("log_dir", "/var/log/sharedos"))
        ops.event("supervisor up marker=%s python=%s" % (marker, sys.version.split()[0]))
        procs = start_services(payload, ops)
        supervise(procs, ops)
    except SystemExit:
        pass
    except CriticalServiceDied as e:
        print("slot failed: %s" % e, file=sys.stderr, flush=True)
        return 7
    except Exception as e:
        print("slot failed: %s: %s" % (type(e).__name__, e), file=sys.stderr, flush=True)
        try:
            import traceback
            traceback.print_exc(file=sys.stderr)
        except Exception:
            pass
        return 5
    finally:
        try:
            print("shutdown: %s" % shutdown(procs), flush=True)
        except Exception as e:
            print("shutdown failed: %s: %s" % (type(e).__name__, e), file=sys.stderr, flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
