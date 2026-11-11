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

Lab verifiers (docs/custom-tasks.md): a task whose evaluator is {"func": "drums_verifier", "verifier": NAME,
"function": FN, "options": {...}} is scored by FN in NAME.py from the verifiers directory (default
/etc/drums/verifiers, `osw.verifiers_dir` in drumsd.json). Only files the host's root owns and nobody else can write
are loaded: the task JSON (which any API client can send) names a verifier, it never carries code. FN runs here, as
this unprivileged helper user, outside the desktop; it reads the desktop through `Ctx` (the slot's own guest server).

No privilege escalation for the agent: the desktop user has no sudo in a slot (no administrator group, no_new_privs on
every process it drives). A setup or postconfig step whose command runs `sudo` (OSWorld writes `echo {CLIENT_PASSWORD}
| sudo -S ...`) is trusted provisioning: it is sent to drumsd (`provision_url`, a one-time capability for this call),
which runs it in the slot as the user with sudo for that step only. Every other step goes to the guest server as before.

`upload_file` reads only files under the assets directory (`assets_dir`, default /var/lib/drums/assets; symlinks are
resolved first): a task names a host file, and any API client can send a task.
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
DEFAULT_ASSETS_DIR = "/var/lib/drums/assets"
VERIFIER_FUNC = "drums_verifier"
DEFAULT_VERIFIERS_DIR = "/etc/drums/verifiers"
TRUSTED_UID = 0                 # lab verifiers must be owned by root (tests point this at their own uid)


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
        if is_lab_task(task):           # OSWorld's metrics table has no lab verifier: set the task info without it
            self.task_id = task["id"]
            self.cache_dir = os.path.join(self.cache_dir_base, self.task_id)
            os.makedirs(self.cache_dir, exist_ok=True)
            self.instruction = task.get("instruction", "")
            self.config = task.get("config") or []
            self.evaluator = task["evaluator"]
        else:
            self._set_task_info(task)
        self.setup_controller.reset_cache_dir(self.cache_dir)


# ------------------------------------------------------------------------------------------------ provisioning, assets
def needs_root(command) -> bool:
    """A setup command that runs sudo (the same rule as drumsd's provision::needs_root): a word `sudo` or `.../sudo`."""
    import re
    text = command if isinstance(command, str) else " ".join(str(x) for x in (command or []))
    words = re.split(r"[^A-Za-z0-9_\-/.]+", text)
    return any(w == "sudo" or w.endswith("/sudo") for w in words)


def route_root_steps(ctrl, provision_url):
    """Steps whose command runs sudo go to drumsd's provisioning endpoint instead of the desktop's guest server (same
    request and reply form as the guest's /setup/execute). Covers `execute`, `command` and OSWorld's own internal sudo
    calls (they all go through _execute_setup). Without a provisioning URL they go to the guest, where sudo refuses."""
    orig = getattr(ctrl, "_execute_setup", None)
    if orig is None:            # a controller without it has no execute/command steps to route
        return

    def _execute_setup(command, *a, **kw):
        if provision_url and needs_root(command):
            saved = ctrl.http_server
            ctrl.http_server = provision_url
            try:
                return orig(command, *a, **kw)
            finally:
                ctrl.http_server = saved
        return orig(command, *a, **kw)
    ctrl._execute_setup = _execute_setup


def restrict_uploads(ctrl, assets_dir):
    """`upload_file` copies a host file into the desktop: only a regular file under assets_dir (after resolving symlinks)."""
    orig = getattr(ctrl, "_upload_file_setup", None)
    if orig is None:            # no upload_file step exists on such a controller (setup() asserts the method)
        return
    root = os.path.realpath(assets_dir)

    def _upload_file_setup(files, *a, **kw):
        checked = []
        for f in files:
            lp = f.get("local_path")
            if not isinstance(lp, str) or not lp:
                raise ValueError("upload_file: local_path must be a host path under the assets directory")
            real = os.path.realpath(lp)
            if real == root or os.path.commonpath([real, root]) != root:
                raise PermissionError(f"upload_file: {lp!r} is outside the assets directory {assets_dir} "
                                      "(the operator installs a task's host files there: docs/custom-tasks.md)")
            if not os.path.isfile(real):
                raise FileNotFoundError(f"upload_file: {lp!r} is not a regular file under {assets_dir}")
            checked.append(dict(f, local_path=real))
        return orig(checked, *a, **kw)
    ctrl._upload_file_setup = _upload_file_setup


# ------------------------------------------------------------------------------------------------ lab verifiers
def is_lab_task(task) -> bool:
    return isinstance(task.get("evaluator"), dict) and task["evaluator"].get("func") == VERIFIER_FUNC


class Ctx:
    """What a lab verifier sees of the episode's desktop. Everything goes through the slot's own guest server, so a
    verifier reads exactly the desktop it scores (and runs commands there as the desktop user, never as root)."""

    def __init__(self, host, port, task, options, workdir, timeout=120):
        self.base = f"http://{host}:{port}"
        self.task, self.options, self.workdir, self.timeout = task, options, workdir, timeout

    def read_file(self, path: str):
        """The file's bytes, or None if it does not exist (HTTP 404). Any other failure raises: a checker error,
        never a silent 0."""
        import requests
        r = requests.post(self.base + "/file", data={"file_path": path}, timeout=self.timeout)
        if r.status_code == 404:
            return None
        r.raise_for_status()
        return r.content

    def run(self, command, shell: bool = False, timeout: float | None = None) -> dict:
        """Run a command in the desktop as its user; returns {"returncode", "output", "error"} (no exception on a
        non-zero exit: the exit status is often the answer)."""
        import requests
        r = requests.post(self.base + "/execute", json={"command": command, "shell": shell}, timeout=timeout or self.timeout)
        r.raise_for_status()
        v = r.json()
        return {"returncode": v.get("returncode"), "output": v.get("output", ""), "error": v.get("error", "")}


def load_verifier(vdir: str, name: str):
    """NAME.py from vdir, only if the directory and the file are owned by root and not writable by group or others."""
    import importlib.util
    import re
    import stat
    if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]{0,63}", name):
        raise ValueError(f"verifier name {name!r}: letters, digits and _ only")
    path = os.path.join(vdir, name + ".py")
    for p in (vdir, path):
        try:
            st = os.stat(p)
        except FileNotFoundError:
            raise FileNotFoundError(f"no lab verifier {path} (install it as root: docs/custom-tasks.md)") from None
        if st.st_uid != TRUSTED_UID or st.st_mode & (stat.S_IWGRP | stat.S_IWOTH):
            raise PermissionError(f"{p} must be owned by root and not group/world-writable (it scores rewards)")
    spec = importlib.util.spec_from_file_location(f"drums_lab_verifier_{name}", path)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def lab_score(out) -> tuple:
    """A verifier's return value -> (score, info). A number (or bool), or {"score": number, "info": {...}}."""
    info = None
    if isinstance(out, dict):
        out, info = out.get("score"), out.get("info")
    if isinstance(out, bool):
        out = float(out)
    if not isinstance(out, (int, float)) or out != out or out in (float("inf"), float("-inf")):
        raise TypeError(f"a verifier must return a finite number or {{'score': number}}, got {out!r}")
    return float(out), info


def lab_function(req, verifier, function):
    mod = load_verifier(req.get("verifiers_dir") or DEFAULT_VERIFIERS_DIR, verifier)
    fn = getattr(mod, function or "reward", None)
    if not callable(fn):
        raise AttributeError(f"verifier {verifier!r} has no function {function or 'reward'!r}")
    return fn


def run_lab_verifier(env, task, req) -> dict:
    ev = task["evaluator"]
    if ev.get("postconfig"):
        if env.setup_controller.setup(ev["postconfig"], False) is False:
            raise RuntimeError("Postconfig setup failed")
    fn = lab_function(req, ev.get("verifier"), ev.get("function"))
    ctx = Ctx(req["host"], req["server_port"], task, ev.get("options") or {}, env.cache_dir)
    return with_deadline(checker_deadline(ev), lambda: lab_score(fn(ctx, **(ev.get("options") or {}))))


# ------------------------------------------------------------------------------------------------ reward audit
# Every evaluate reply carries `audit` (drumsd returns it as the reward's `audit`): how far the score can be trusted,
# measured on this rollout, never assumed. Each check runs only when it can run without changing the scored state:
#   checker    the first call's seconds and error class (none | exception | timeout | bad_return)
#   recheck    the same checker run a second time on the same state (lab verifiers always; OSWorld evaluators only
#              without postconfig, whose actions could change the state); a different score is `nondeterministic`
#   redundant  an independent second checker the task declares (`evaluator.redundant`: verifier, function,
#              options); a different score is `redundant_disagree`
#   scope      files under the task's declared roots compared with its start manifest (`evaluator.scope`: roots,
#              manifest {path: sha256}, allow [globs], ignore [globs]); a change outside `allow` is `scope_violation`,
#              a reward-hacking signal (the agent edited state the task did not ask for)
# The scope scan runs in the desktop as the desktop user (stdlib python3): it is a signal, not a security boundary.
AUDIT_VERSION = 1
DEFAULT_CHECKER_DEADLINE_S = 90.0


class CheckerTimeout(Exception):
    pass


def checker_deadline(ev) -> float:
    try:
        return float((ev or {}).get("deadline_s") or DEFAULT_CHECKER_DEADLINE_S)
    except (TypeError, ValueError):
        return DEFAULT_CHECKER_DEADLINE_S


def with_deadline(seconds, fn):
    """fn() with a wall-clock limit (SIGALRM; this helper is single-threaded). A checker over its limit raises
    CheckerTimeout: a checker error of class `timeout`, never a score."""
    import signal
    if seconds <= 0 or not hasattr(signal, "SIGALRM"):
        return fn()

    def on_alarm(signum, frame):  # noqa: ARG001
        raise CheckerTimeout(f"checker exceeded its {seconds:g} s deadline")
    old = signal.signal(signal.SIGALRM, on_alarm)
    signal.setitimer(signal.ITIMER_REAL, seconds)
    try:
        return fn()
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, old)


def error_class(e) -> str:
    if e is None:
        return "none"
    if isinstance(e, CheckerTimeout):
        return "timeout"
    if isinstance(e, TypeError) and "a verifier must return" in str(e):
        return "bad_return"
    return "exception"


SCOPE_SCAN = r"""
import fnmatch, hashlib, json, os, sys
req = json.loads(sys.argv[1]); out = {}
for root in req["roots"]:
    if not os.path.isdir(root):
        continue
    for d, dirs, files in os.walk(root):
        dirs[:] = sorted(x for x in dirs if not any(fnmatch.fnmatch(os.path.join(d, x), g) for g in req["ignore"]))
        for f in sorted(files):
            p = os.path.join(d, f)
            if any(fnmatch.fnmatch(p, g) or fnmatch.fnmatch(f, g) for g in req["ignore"]):
                continue
            try:
                if os.path.islink(p) or not os.path.isfile(p):
                    out[p] = "special"; continue
                if os.path.getsize(p) > req["max_bytes"]:
                    out[p] = "size:%d" % os.path.getsize(p); continue
                h = hashlib.sha256()
                with open(p, "rb") as fh:
                    for b in iter(lambda: fh.read(1 << 20), b""):
                        h.update(b)
                out[p] = h.hexdigest()
            except OSError as e:
                out[p] = "error:%s" % e.__class__.__name__
print(json.dumps(out))
"""


def scope_check(ctx, scope) -> dict:
    """Compare the files under scope.roots with scope.manifest. Paths in the manifest are absolute."""
    import fnmatch
    roots = [r.rstrip("/") for r in scope.get("roots") or []]
    manifest = scope.get("manifest") or {}
    allow = list(scope.get("allow") or [])
    ignore = list(scope.get("ignore") or []) + [".~lock.*#", "*/__pycache__"]
    r = ctx.run(["python3", "-I", "-c", SCOPE_SCAN, json.dumps({"roots": roots, "ignore": ignore, "max_bytes": 64 << 20})],
                timeout=60)
    if r.get("returncode") != 0:
        raise RuntimeError(f"scope scan failed: rc {r.get('returncode')}: {(r.get('error') or '')[:300]}")
    now = json.loads((r.get("output") or "").strip().splitlines()[-1])
    allowed = lambda p: any(fnmatch.fnmatch(p, g) for g in allow)  # noqa: E731
    changed = sorted(p for p, h in now.items() if p in manifest and manifest[p] != h and not allowed(p))
    deleted = sorted(p for p in manifest if p not in now and not allowed(p))
    created = sorted(p for p in now if p not in manifest and not allowed(p))
    return {"ran": True, "roots": roots, "files": len(now), "changed": changed[:50], "deleted": deleted[:50],
            "created": created[:50], "violations": len(changed) + len(deleted) + len(created)}


def audit_evaluate(env, task, req, score, err, t_first) -> dict:
    ev = task.get("evaluator") or {}
    flags = []
    a = {"version": AUDIT_VERSION, "checker": {"seconds": round(t_first, 3), "error_class": error_class(err)},
         "recheck": {"ran": False}, "redundant": {"ran": False}, "scope": {"ran": False}}
    if err is not None:
        flags.append("checker_timeout" if isinstance(err, CheckerTimeout) else "checker_crash")
    lab = is_lab_task(task)
    ctx = Ctx(req["host"], req["server_port"], task, ev.get("options") or {}, env.cache_dir) if lab or ev.get("scope") else None
    # recheck: the same checker again on the same state
    if err is None and (ev.get("audit_recheck", True) is not False):
        if lab or not ev.get("postconfig"):
            t0 = time.monotonic()
            try:
                if lab:
                    fn = lab_function(req, ev.get("verifier"), ev.get("function"))
                    s2, _ = with_deadline(checker_deadline(ev), lambda: lab_score(fn(ctx, **(ev.get("options") or {}))))
                else:
                    s2 = float(with_deadline(checker_deadline(ev), env._evaluate))
                a["recheck"] = {"ran": True, "score": s2, "agree": s2 == score, "seconds": round(time.monotonic() - t0, 3)}
                if s2 != score:
                    flags.append("nondeterministic")
            except Exception as e:  # noqa: BLE001
                a["recheck"] = {"ran": True, "score": None, "agree": False, "error": f"{type(e).__name__}: {e}"[:300],
                                "error_class": error_class(e), "seconds": round(time.monotonic() - t0, 3)}
                flags.append("nondeterministic")
        else:
            a["recheck"] = {"ran": False, "skipped": "postconfig (a second run could change the state)"}
    # redundant: an independent checker the task declares
    red = ev.get("redundant")
    if err is None and isinstance(red, dict) and lab:
        t0 = time.monotonic()
        try:
            fn = lab_function(req, red.get("verifier") or ev.get("verifier"), red.get("function"))
            opts = red.get("options") if red.get("options") is not None else (ev.get("options") or {})
            rctx = Ctx(req["host"], req["server_port"], task, opts, env.cache_dir)
            s3, info3 = with_deadline(checker_deadline(ev), lambda: lab_score(fn(rctx, **opts)))
            a["redundant"] = {"ran": True, "checker": f"{red.get('verifier') or ev.get('verifier')}.{red.get('function')}",
                              "score": s3, "agree": s3 == score, "seconds": round(time.monotonic() - t0, 3)}
            if s3 != score:
                flags.append("redundant_disagree")
        except Exception as e:  # noqa: BLE001
            a["redundant"] = {"ran": True, "score": None, "agree": False, "error": f"{type(e).__name__}: {e}"[:300],
                              "error_class": error_class(e), "seconds": round(time.monotonic() - t0, 3)}
            flags.append("redundant_disagree")
    elif red is not None and not lab:
        a["redundant"] = {"ran": False, "skipped": "redundant checkers are lab verifiers"}
    # scope: state edits outside what the task asked for
    if isinstance(ev.get("scope"), dict):
        t0 = time.monotonic()
        try:
            a["scope"] = scope_check(ctx, ev["scope"])
            a["scope"]["seconds"] = round(time.monotonic() - t0, 3)
            if a["scope"]["violations"]:
                flags.append("scope_violation")
        except Exception as e:  # noqa: BLE001
            a["scope"] = {"ran": True, "error": f"{type(e).__name__}: {e}"[:300], "seconds": round(time.monotonic() - t0, 3)}
            flags.append("scope_unchecked")
    a["flags"] = flags
    return a


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
    route_root_steps(env.setup_controller, req.get("provision_url"))
    restrict_uploads(env.setup_controller, req.get("assets_dir") or DEFAULT_ASSETS_DIR)
    try:
        return run_verb(verb, env, task, staged, t0, req)
    finally:
        shutil.rmtree(env.cache_dir, ignore_errors=True)


def run_verb(verb, env, task, staged, t0, req=None) -> dict:
    if verb == "setup":
        ok = env.setup_controller.setup(env.config, False)
        r = {"ok": bool(ok), "seconds": round(time.monotonic() - t0, 3), "n_steps": len(env.config), "staged": len(staged)}
        if not ok:
            r["error"] = "OSWorld setup returned False (a config step failed; see the helper log)"
        return r
    info = None
    t1 = time.monotonic()
    exc = None
    try:
        if is_lab_task(task):
            score, info = run_lab_verifier(env, task, req)
        else:
            score = with_deadline(checker_deadline(task.get("evaluator")), env._evaluate)
        err = None
    except Exception as e:  # a checker error is its own outcome, never a score
        score, err, exc = None, f"{type(e).__name__}: {e}"[:800], e
    t_first = time.monotonic() - t1
    score = None if score is None else float(score)
    ev = task.get("evaluator") or {}
    try:
        audit = audit_evaluate(env, task, req, score, exc, t_first)
    except Exception as e:  # noqa: BLE001 - the audit never turns a score into an error
        audit = {"version": AUDIT_VERSION, "error": f"{type(e).__name__}: {e}"[:300], "flags": ["audit_error"]}
    out = {"ok": True, "score": score, "checker_error": err,
           "seconds": round(time.monotonic() - t0, 3), "osworld_commit": OSWORLD_COMMIT,
           "postconfig_ran": bool(ev.get("postconfig")), "audit": audit}
    if is_lab_task(task):
        out["verifier"] = f"{ev.get('verifier')}.{ev.get('function') or 'reward'}"
        out["info"] = info
    return out


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
