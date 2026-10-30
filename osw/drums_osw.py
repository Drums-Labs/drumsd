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
per-task lock with atomic renames, so concurrent slots of one task never read a half-written file.
"""
from __future__ import annotations

import fcntl
import hashlib
import json
import logging
import os
import sys
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
        return {"ok": False, "error": f"usage: drums_osw.py setup|evaluate|stage < request.json (got {verb!r})"}
    req = json.load(sys.stdin)
    task = req["task"]
    t0 = time.monotonic()
    staged = stage(task, req["cache_dir"])
    if verb == "stage":
        return {"ok": True, "staged": staged, "seconds": round(time.monotonic() - t0, 3)}
    env = OswEnv(req["osworld_dir"], req["host"], req["server_port"], req.get("chromium_port"), req.get("vlc_port"),
                 req["cache_dir"], req.get("client_password", "password"))
    env.load(task)
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


if __name__ == "__main__":
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
