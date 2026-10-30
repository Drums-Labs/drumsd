"""OSWorld task setup and checking for drumsd, run by drumsd as an unprivileged user with the OSWorld venv's python.

    drums_osw.py setup     < request.json     run the task's `config` steps against the slot's guest server
    drums_osw.py evaluate  < request.json     run the task's evaluator (postconfig, getters, metric) -> score

request: {"task": {...OSWorld task JSON...}, "host": "127.0.0.1", "server_port": P, "chromium_port": C, "vlc_port": V,
          "cache_dir": DIR, "osworld_dir": DIR, "client_password": "password"}
reply (last stdout line): {"ok": true, ...} or {"ok": false, "error": "..."}

The setup steps, getters, metrics, postconfig and conjunction logic are OSWorld's own code at the pinned commit
(DesktopEnv._set_task_info / evaluate bound to a small object that carries the attributes they read), as in the
final-comparison harness (iarun/osw.py). Differences from DesktopEnv.reset(): no provider (drumsd restores the slot),
and setup runs once (no retry loop): a failed setup is reported as a failure, never retried silently.

Downloads (setup `download` steps and `cloud_file` expected files) are staged into the shared cache first, under a
per-task lock with atomic renames, so concurrent slots of one task never read a half-written file. Staged files are
made read-only.

Each run then works in its own private cache directory (`<cache>/.runs/<task8>-XXXX`, removed at the end) holding
hard links to the staged files only. OSWorld's getters write what they fetch from the desktop (`vm_file`, ...) into
the task's cache directory under a fixed name; with one directory shared by every slot of a task, two slots
verifying at once overwrote each other's file, and a slot could be scored on another slot's desktop.
"""
from __future__ import annotations

import fcntl
import hashlib
import json
import logging
import os
import shutil
import sys
import tempfile
import time
import traceback
import urllib.request
import uuid

OSWORLD_COMMIT = "044bc33901cb893808e3f91ebc9a17ed8e54b44c"


def as_list(x):
    return x if isinstance(x, list) else ([] if x is None else [x])


def stage(task: dict, cache_dir: str) -> list:
    """Every file the task's setup/postconfig downloads or its evaluator fetches (OSWorld cache layout)."""
    tdir = os.path.join(cache_dir, task["id"])
    os.makedirs(tdir, exist_ok=True)
    items = []
    ev = task.get("evaluator") or {}
    for c in (task.get("config") or []) + (ev.get("postconfig") or []):
        if c.get("type") == "download":
            for f in c["parameters"]["files"]:
                items.append((f"{uuid.uuid5(uuid.NAMESPACE_URL, f['url'])}_{os.path.basename(f['path'])}", f["url"]))
    for g in as_list(ev.get("expected")) + as_list(ev.get("result")):
        if isinstance(g, dict) and g.get("type") == "cloud_file":
            for u, d in zip(as_list(g["path"]), as_list(g["dest"])):
                items.append((os.path.basename(d), u))
    out = []
    with open(os.path.join(tdir, ".stage.lock"), "w") as lk:
        fcntl.flock(lk, fcntl.LOCK_EX)
        for name, url in items:
            dst = os.path.join(tdir, name)
            if not os.path.exists(dst):
                err = None
                for i in range(3):
                    try:
                        with urllib.request.urlopen(url, timeout=300) as r, open(dst + ".part", "wb") as f:
                            while True:
                                b = r.read(1 << 20)
                                if not b:
                                    break
                                f.write(b)
                        os.chmod(dst + ".part", 0o444)      # shared by every run: never written again
                        os.rename(dst + ".part", dst)
                        err = None
                        break
                    except Exception as e:  # noqa: BLE001
                        err = f"{type(e).__name__}: {e}"
                        time.sleep(2 * (i + 1))
                if err:
                    raise RuntimeError(f"download {url}: {err}")
            h = hashlib.sha256(open(dst, "rb").read()).hexdigest()
            out.append({"name": name, "url": url, "bytes": os.path.getsize(dst), "sha256": h})
    return out


def private_cache(cache_dir: str, task_id: str, staged: list) -> str:
    """A fresh directory for one run of one slot: hard links to the task's staged files and nothing else (no file
    another run fetched from its desktop). Getters write their fetched files here, so concurrent runs never share one."""
    runs = os.path.join(cache_dir, ".runs")
    os.makedirs(runs, exist_ok=True)
    d = tempfile.mkdtemp(prefix=f"{task_id[:8]}-", dir=runs)
    tdir = os.path.join(cache_dir, task_id)
    for it in staged:
        src, dst = os.path.join(tdir, it["name"]), os.path.join(d, it["name"])
        try:
            os.link(src, dst)
        except OSError:
            shutil.copy2(src, dst)
    return d


class OswEnv:
    def __init__(self, osworld_dir, ip, server_port, chromium_port, vlc_port, cache_base, client_password, width=1920, height=1080):
        os.chdir(osworld_dir)      # setup.py reads evaluation_examples/settings/... relative to the repo
        if osworld_dir not in sys.path:
            sys.path.insert(0, osworld_dir)
        from desktop_env.controllers.python import PythonController
        from desktop_env.controllers.setup import SetupController
        from desktop_env.desktop_env import DesktopEnv
        self.vm_ip, self.server_port, self.chromium_port, self.vlc_port = ip, server_port, chromium_port, vlc_port
        self.cache_dir_base = cache_base
        self.client_password = client_password
        self.screen_width, self.screen_height = width, height
        self.os_type = "Ubuntu"
        self.enable_proxy = False
        self.current_use_proxy = False
        self.is_environment_used = False
        self.action_history = []
        self.provider_name = "drums"
        self.controller = PythonController(vm_ip=ip, server_port=server_port)
        self.setup_controller = SetupController(vm_ip=ip, server_port=server_port, chromium_port=chromium_port, vlc_port=vlc_port,
                                                cache_dir=cache_base, client_password=client_password, screen_width=width,
                                                screen_height=height, os_type="Ubuntu")
        self._set_task_info = DesktopEnv._set_task_info.__get__(self)
        self._set_evaluator_info = DesktopEnv._set_evaluator_info.__get__(self)
        self._evaluate = DesktopEnv.evaluate.__get__(self)

    @property
    def vm_platform(self):
        return self.controller.get_vm_platform()

    @property
    def vm_machine(self):
        return self.controller.get_vm_machine()

    def load(self, task):
        self._set_task_info(task)
        self.setup_controller.reset_cache_dir(self.cache_dir)


def main(argv) -> dict:
    verb = argv[1] if len(argv) > 1 else ""
    if verb not in ("setup", "evaluate", "stage"):
        return {"ok": False, "error": f"usage: drums_osw.py setup|evaluate|stage|serve < request.json (got {verb!r})"}
    req = json.load(sys.stdin)
    if verb != "stage" and not os.environ.get("DRUMS_OSW_NO_SERVER"):
        out = via_server(verb, req)
        if out is not None:
            return out
    return run_request(verb, req)


def run_request(verb, req) -> dict:
    task = req["task"]
    t0 = time.monotonic()
    staged = stage(task, req["cache_dir"])
    if verb == "stage":
        return {"ok": True, "staged": staged, "seconds": round(time.monotonic() - t0, 3)}
    env = OswEnv(req["osworld_dir"], req["host"], req["server_port"], req.get("chromium_port"), req.get("vlc_port"),
                 req["cache_dir"], req.get("client_password", "password"))
    env.load(task)
    env.cache_dir = private_cache(req["cache_dir"], task["id"], staged)
    env.setup_controller.reset_cache_dir(env.cache_dir)
    try:
        return run_verb(verb, env, task, staged, t0)
    finally:
        shutil.rmtree(env.cache_dir, ignore_errors=True)


def run_verb(verb, env, task, staged, t0) -> dict:
    if verb == "setup":
        ok = env.setup_controller.setup(env.config, False)
        r = {"ok": bool(ok), "seconds": round(time.monotonic() - t0, 3), "n_steps": len(env.config), "staged": len(staged)}
        if not ok:
            r["error"] = "OSWorld setup returned False (a config step failed; see the helper log)"
        return r
    try:
        score = env._evaluate()
        err = None
    except Exception as e:  # a checker error is its own outcome, never a score
        score, err = None, f"{type(e).__name__}: {e}"[:800]
    ev = task.get("evaluator") or {}
    return {"ok": True, "score": None if score is None else float(score), "checker_error": err,
            "seconds": round(time.monotonic() - t0, 3), "osworld_commit": OSWORLD_COMMIT,
            "postconfig_ran": bool(ev.get("postconfig"))}


# ------------------------------------------------------------------------------------------------ fork server
# Importing OSWorld's controllers, getters and metrics costs about 4 s and 12 CPU-seconds per helper process; at 128+
# envs the checkers' imports alone needed more cores than the host has. `serve` imports once and forks a worker per
# request (copy-on-write; nothing is shared between requests but the read-only imported code). The client side is
# this same script run by drumsd as before: it hands its request to the server if the socket answers, else does the
# work itself. A worker dies with its client: if the client goes away (drumsd's timeout kills it), the worker's
# process group is killed.
SOCK = "/run/drums-osw/osw.sock"


def via_server(verb, req, sock_path=SOCK):
    import socket
    try:
        c = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        c.connect(sock_path)
    except OSError:
        return None
    with c:
        c.sendall(json.dumps({"verb": verb, "req": req}).encode() + b"\n")
        buf = b""
        while True:
            b = c.recv(1 << 16)
            if not b:
                break
            buf += b
    line = buf.decode(errors="replace").strip().splitlines()
    if not line:
        return {"ok": False, "error": "osw server closed the connection without a reply"}
    return json.loads(line[-1])


def _worker(verb, req, wfd, efd):
    os.setpgid(0, 0)
    os.dup2(efd, 2)
    sys.stdout = sys.stderr = os.fdopen(2, "w", buffering=1)
    logging.basicConfig(stream=sys.stderr, level=logging.INFO, force=True)
    try:
        out = run_request(verb, req)
    except Exception as e:  # noqa: BLE001
        out = {"ok": False, "error": f"{type(e).__name__}: {e}"[:1200], "trace": traceback.format_exc()[-2000:]}
    os.write(wfd, (json.dumps(out, default=str) + "\n").encode())
    os._exit(0)


def _handle(conn):
    import select
    import signal
    signal.signal(signal.SIGCHLD, signal.SIG_DFL)
    f = conn.makefile("rb")
    msg = json.loads(f.readline())
    r, w = os.pipe()
    err = tempfile.TemporaryFile()
    pid = os.fork()
    if pid == 0:
        os.close(r)
        conn.close()
        _worker(msg["verb"], msg["req"], w, err.fileno())
    os.close(w)
    reply = b""
    while True:
        rd, _, _ = select.select([r, conn], [], [])
        if conn in rd and not conn.recv(1, __import__("socket").MSG_PEEK):
            os.killpg(pid, signal.SIGKILL)      # the client went away: so does the work
            os.waitpid(pid, 0)
            return
        if r in rd:
            b = os.read(r, 1 << 16)
            if not b:
                break
            reply += b
    os.waitpid(pid, 0)
    try:
        os.killpg(pid, signal.SIGKILL)          # anything the worker left running
    except OSError:
        pass
    if not reply.strip():
        err.seek(0)
        tail = err.read()[-1200:].decode(errors="replace")
        reply = (json.dumps({"ok": False, "error": f"osw worker exited without a reply; stderr: {tail}"}) + "\n").encode()
    conn.sendall(reply)


def serve(argv):
    import signal
    import socket
    sock_path = argv[argv.index("--sock") + 1] if "--sock" in argv else SOCK
    osworld_dir = argv[argv.index("--osworld-dir") + 1]
    os.chdir(osworld_dir)
    sys.path.insert(0, osworld_dir)
    import desktop_env.controllers.python  # noqa: F401
    import desktop_env.controllers.setup  # noqa: F401
    import desktop_env.desktop_env  # noqa: F401
    import desktop_env.evaluators.getters  # noqa: F401
    import desktop_env.evaluators.metrics  # noqa: F401
    signal.signal(signal.SIGCHLD, signal.SIG_IGN)      # connection handlers are reaped by the kernel
    try:
        os.unlink(sock_path)
    except FileNotFoundError:
        pass
    srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    srv.bind(sock_path)
    os.chmod(sock_path, 0o600)
    srv.listen(1024)
    print(f"drums_osw serve: OSWorld imported, listening on {sock_path}", file=sys.stderr, flush=True)
    while True:
        conn, _ = srv.accept()
        if os.fork() == 0:
            srv.close()
            try:
                _handle(conn)
            finally:
                os._exit(0)
        conn.close()


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "serve":
        logging.basicConfig(stream=sys.stderr, level=logging.WARNING)
        serve(sys.argv)
        sys.exit(1)
    # OSWorld logs and prints; keep stdout for the one reply line
    real_stdout = sys.stdout
    sys.stdout = sys.stderr
    logging.basicConfig(stream=sys.stderr, level=logging.INFO)
    for n in ("urllib3", "httpx", "PIL"):
        logging.getLogger(n).setLevel(logging.WARNING)
    try:
        out = main(sys.argv)
    except Exception as e:  # noqa: BLE001
        out = {"ok": False, "error": f"{type(e).__name__}: {e}"[:1200], "trace": traceback.format_exc()[-2000:]}
    real_stdout.write(json.dumps(out, default=str) + "\n")
    real_stdout.flush()
