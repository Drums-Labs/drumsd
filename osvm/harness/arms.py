"""Arm drivers for the T5 study. Every arm exposes the same operations to soak.py:

  up(n) -> list of desktop handles          down()
  start_episode(d, task) -> dict            (reset to the arm's own clean/start state, then the task's setup)
  act(d, action) -> dict                    one action (timed by the caller)
  screenshot(d) -> (bytes, seconds)         the arm's own observation path
  shell(d, cmd) -> (rc, stdout)             a command in the desktop (checkers, oracle shell steps, audit probe)
  verify(d, task) -> dict                   reward + validity

Drums arms go through the drumsd API only. Incumbent arms are run as their own docs say (AndroidWorld Docker
image and server; WindowsAgentArena container and in-VM server); our audit (state probe, canary) runs next to them
and is never counted in their latency.
"""
import base64, hashlib, json, os, subprocess, threading, time, urllib.request, urllib.error
from checker import judge

def http(method, url, body=None, token=None, timeout=600, raw=False):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url, data=data, method=method)
    if body is not None:
        req.add_header("Content-Type", "application/json")
    if token:
        req.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            b = r.read()
            if raw or not b:
                return r.status, (b if raw else None)
            try:
                return r.status, json.loads(b)
            except ValueError:
                return r.status, {"text": b.decode(errors="replace")[:500]}
    except urllib.error.HTTPError as e:
        b = e.read()
        try:
            return e.code, json.loads(b)
        except Exception:
            return e.code, {"error": b.decode(errors="replace")[:500]}
    except (urllib.error.URLError, ConnectionError, TimeoutError, OSError) as e:
        # not answering (yet): status 0, the caller decides
        return 0, {"error": f"{type(e).__name__}: {e}"}

def run(args, timeout=120, stdin=None):
    t = time.time()
    try:
        p = subprocess.run(args, capture_output=True, timeout=timeout, input=stdin)
        return p.returncode, p.stdout.decode(errors="replace").replace("\r\n", "\n"), p.stderr.decode(errors="replace"), time.time() - t
    except subprocess.TimeoutExpired:
        return -2, "", "timeout", time.time() - t

def sha(s):
    return hashlib.sha256(s.encode()).hexdigest()


class DrumsArm:
    """A drumsd serve-os pool (android-redroid, android-emulator or windows-qemu)."""
    def __init__(self, url, token, image):
        self.url, self.token, self.image = url.rstrip("/"), token, image
        self.ids = []
    def up(self, n, parallel=8):
        out, lock = [], threading.Lock()
        def one(_):
            t = time.time()
            st, v = http("POST", f"{self.url}/v1/slots", {"checkpoint": self.image}, self.token)
            with lock:
                out.append({"status": st, "view": v, "wall_s": time.time() - t})
        ths = []
        for i in range(n):
            th = threading.Thread(target=one, args=(i,)); th.start(); ths.append(th)
            if len(ths) >= parallel:
                [x.join() for x in ths]; ths = []
        [x.join() for x in ths]
        self.ids = [o["view"]["id"] for o in out if o["status"] == 201]
        return out
    def start_episode(self, d, task):
        t = time.time()
        st, v = http("POST", f"{self.url}/v1/slots/{d}/task", {"task": task}, self.token)
        wall = time.time() - t
        if st == 409:  # quarantined from an earlier episode: release, reset, then load the task
            http("POST", f"{self.url}/v1/slots/{d}/release", {}, self.token)
            t = time.time()
            st, v = http("POST", f"{self.url}/v1/slots/{d}/task", {"task": task}, self.token)
            wall = time.time() - t
        ok = st == 200 and v.get("state") == "ready"
        cost = (v or {}).get("cost", {}) if isinstance(v, dict) else {}
        return {"ok": ok, "http": st, "wall_s": wall, "ready_s": cost.get("ready_s"), "setup_s": cost.get("setup_s"),
                "reset_timing": cost.get("timing"), "gates": (v or {}).get("gates") if isinstance(v, dict) else None,
                "state": (v or {}).get("state") if isinstance(v, dict) else None, "reason": (v or {}).get("reason") if isinstance(v, dict) else str(v)}
    def act(self, d, action, screenshot=True):
        t = time.time()
        st, v = http("POST", f"{self.url}/v1/slots/{d}/step", {"action": action, "screenshot": screenshot}, self.token)
        wall = time.time() - t
        obs = (v or {}).get("observation", {}) if st == 200 else {}
        return {"ok": st == 200, "http": st, "wall_s": wall, "act_s": (v or {}).get("seconds"), "shot_s": obs.get("seconds"),
                "shot_bytes": obs.get("bytes"), "error": None if st == 200 else v}
    def screenshot(self, d):
        t = time.time()
        st, b = http("GET", f"{self.url}/v1/slots/{d}/screenshot", token=self.token, raw=True)
        return (b if st == 200 else None), time.time() - t
    def verify(self, d, task):
        t = time.time()
        st, v = http("POST", f"{self.url}/v1/slots/{d}/verify", {}, self.token)
        return {"ok": st == 200, "http": st, "wall_s": time.time() - t, "reward": (v or {}).get("reward"), "valid": (v or {}).get("valid"),
                "verdict": (v or {}).get("verdict"), "checks": (v or {}).get("checks"), "gates": (v or {}).get("gates")}
    def shell_action(self, cmd):
        return {"action_type": "shell", "command": cmd} if "android" in self.image else {"type": "powershell", "command": cmd}
    def down(self):
        for d in self.ids:
            http("DELETE", f"{self.url}/v1/slots/{d}", token=self.token)


class _AuditMixin:
    """State probe + canary for arms whose reset has no gate of its own (our audit; not counted in their latency)."""
    def probe_text(self, d):
        return self.shell(d, self.probe)
    def audit_after_reset(self, d, ep):
        t = time.time()
        rc, out = self.probe_text(d)
        if rc != 0 and not out.strip():
            return {"state_hash_ok": False, "state_hash": None, "clean_hash": self.clean_hash.get(d), "canary_written": False, "audit_s": time.time() - t, "probe_error": f"probe exit {rc}"}
        h = sha(out)
        clean = self.clean_hash.get(d)
        if clean is None:
            self.clean_hash[d] = clean = h
        mine = f"drums_canary_{d}_{ep}"
        rc2, _ = self.shell(d, self.canary_write.format(name=mine))
        return {"state_hash_ok": h == clean, "state_hash": h, "clean_hash": clean, "canary_written": rc2 == 0, "audit_s": time.time() - t, "probe_text": None if h == clean else out[:20000]}
    def audit_end(self, d, ep):
        """Isolation for incumbents: no canary of ANOTHER desktop is present and this episode's canary is. Canaries
        left by this desktop's own earlier episodes are stale state, counted by the clean-start metric instead."""
        rc, out = self.shell(d, self.canary_list)
        names = sorted(x.strip().split("/")[-1].split("\\")[-1] for x in out.split() if "drums_canary_" in x)
        mine = f"drums_canary_{d}_{ep}"
        foreign = [n for n in names if not n.startswith(f"drums_canary_{d}_")]
        return (mine in names and not foreign), names


class AwArm(_AuditMixin):
    """AndroidWorld's Docker setup (android_world Dockerfile @55ddaf5): one emulator per container (-no-snapshot,
    -memory 2048, -gpu off), its FastAPI server on :5000. Stock reset = POST /reset?go_home=true; the task's own
    setup then sets the preconditions (as AndroidWorld's initialize_task does). Actions: POST /execute_action
    (JSONAction). Observation: GET /screenshot?wait_to_stabilize=false (its stock path: pixels as a JSON list)."""
    def __init__(self, image, probe, port_base=20000, apks=(), configure_cmds=()):
        self.image, self.probe, self.port_base, self.apks, self.configure_cmds = image, probe, port_base, apks, configure_cmds
        self.ids, self.clean_hash = [], {}
        self.canary_write = "touch /data/local/tmp/{name}"
        self.canary_list = "ls /data/local/tmp/ | grep drums_canary_ || true"
    def _url(self, d):
        return f"http://127.0.0.1:{self.port_base + int(d.split('-')[1])}"
    def shell(self, d, cmd, timeout=120):
        rc, out, err, _ = run(["docker", "exec", d, "adb", "-s", "emulator-5554", "shell", cmd], timeout=timeout)
        return rc, out
    def up(self, n, parallel=8):
        out = []
        def one(i):
            d = f"aw-{i}"
            t = time.time()
            run(["docker", "rm", "-f", d])
            rc, o, e, _ = run(["docker", "run", "-d", "--privileged", "--name", d, "-p", f"127.0.0.1:{self.port_base + i}:5000", self.image], timeout=120)
            ok = False
            for _ in range(900):
                st, v = http("GET", self._url(d) + "/health", timeout=5) if rc == 0 else (0, None)
                if st == 200:
                    ok = True; break
                time.sleep(1)
            boot = time.time() - t
            # same apps and settings as the Drums golden images (osvm/hosts/android-golden.sh configure())
            for a in self.apks:
                run(["docker", "cp", a, f"{d}:/tmp/{os.path.basename(a)}"])
                run(["docker", "exec", d, "adb", "-s", "emulator-5554", "install", "-r", "-g", f"/tmp/{os.path.basename(a)}"], timeout=300)
            for c in self.configure_cmds:
                self.shell(d, c)
                if c.startswith("monkey"):
                    time.sleep(6)
            out.append({"id": d, "ok": ok, "boot_s": boot, "ready_s": time.time() - t})
        ths = []
        for i in range(n):
            th = threading.Thread(target=one, args=(i,)); th.start(); ths.append(th)
            if len(ths) >= parallel:
                [x.join() for x in ths]; ths = []
        [x.join() for x in ths]
        self.ids = sorted(o["id"] for o in out if o["ok"])
        return out
    def start_episode(self, d, task, ep):
        t = time.time()
        st, v = http("POST", self._url(d) + "/reset?go_home=true", {}, timeout=300)
        ready = time.time() - t
        audit = self.audit_after_reset(d, ep)
        t2 = time.time()
        ok, detail = st == 200, ""
        for c in task.get("setup", []):
            rc, o = self.shell(d, c)
            if rc != 0:
                ok, detail = False, f"{c!r} exit {rc}: {o[:200]}"; break
        return {"ok": ok, "http": st, "ready_s": ready, "setup_s": time.time() - t2, "audit": audit, "reason": detail}
    def act(self, d, action, screenshot=True):
        t = time.time()
        if action.get("action_type") == "shell":
            rc, o = self.shell(d, action["command"]); st = 200 if rc == 0 else 500
        else:
            st, v = http("POST", self._url(d) + "/execute_action", action, timeout=300)
        act_s = time.time() - t
        shot_s, nbytes, adb_shot_s = None, None, None
        if screenshot:
            t2 = time.time()
            s2, b = http("GET", self._url(d) + "/screenshot?wait_to_stabilize=false", timeout=300, raw=True)
            shot_s, nbytes = time.time() - t2, len(b or b"")
            wall = time.time() - t
            # tuned observation path (not AndroidWorld's API): adb screencap inside the container, timed separately
            t3 = time.time()
            rc, _, _, _ = run(["docker", "exec", d, "sh", "-c", "adb -s emulator-5554 exec-out screencap -p > /dev/null"], timeout=120)
            adb_shot_s = time.time() - t3 if rc == 0 else None
        else:
            wall = time.time() - t
        return {"ok": st == 200, "http": st, "wall_s": wall, "act_s": act_s, "shot_s": shot_s, "shot_bytes": nbytes, "adb_shot_s": adb_shot_s,
                "tuned_wall_s": (act_s + adb_shot_s) if adb_shot_s is not None else None}
    def verify(self, d, task, ep):
        t = time.time()
        checks, ok = [], True
        for c in task["check"]:
            rc, o = self.shell(d, c["cmd"])
            p, det = judge(c, o, rc)
            ok &= p; checks.append({"cmd": c["cmd"], "pass": p, "detail": det})
        iso, names = self.audit_end(d, ep)
        return {"ok": True, "wall_s": time.time() - t, "reward": 1.0 if ok else 0.0, "checks": checks, "isolation_end": iso, "canaries": names}
    def down(self, logdir=None):
        for d in self.ids:
            if logdir:
                rc, o, e, _ = run(["docker", "logs", d], timeout=60)
                open(os.path.join(logdir, f"{d}.docker.log"), "w").write(o + e)
            run(["docker", "rm", "-f", d])
