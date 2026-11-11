"""WindowsAgentArena incumbent arm, run as WAA's own scripts run it (scripts/run.sh + entry.sh, --start-client
false): one windowsarena/winarena container per desktop, QEMU 9.1.1 inside, 8 GiB / 8 vCPU (WAA defaults), the
golden storage copied per desktop (WAA mounts storage read-write; a shared copy would be corrupted).

Stock reset (client/desktop_env/envs/desktop_env.py DesktopEnv.reset for its VM): POST /setup/close_all, sleep 5,
POST /setup/clear_task_files, task setup, sleep 5. No snapshot revert exists in WAA's local path. The audit (state
probe, canary) is ours and not counted in its latency. Observation: GET /screenshot (WAA's server path).
"""
import json, os, subprocess, threading, time
from arms import http, run, sha, _AuditMixin
from checker import judge

WAA_IMAGE = os.environ.get("WAA_IMAGE", "windowsarena/winarena@sha256:869afffe384f0e0356dbfcdf78486611e31d62e692544cbfd9a6c08606f07440")
GOLDEN = os.environ.get("WAA_GOLDEN", "/srv/t5/waa-golden")
STORE = os.environ.get("WAA_STORE", "/srv/t5/waa")
PY = os.environ.get("WAA_PYTHON", r"C:\Users\Docker\AppData\Local\Programs\Python\Python310\python.exe")
PREFIX = "import pyautogui; import time; pyautogui.FAILSAFE = False; "

class WaaArm(_AuditMixin):
    def __init__(self, image, probe, ram="8G", cpus="8", stock_sleeps=True):
        self.image = image or WAA_IMAGE
        self.probe, self.ram, self.cpus, self.stock_sleeps = probe, ram, cpus, stock_sleeps
        self.ids, self.ip, self.clean_hash = [], {}, {}
        self.canary_write = "New-Item -ItemType File -Force -Path C:\\Users\\Public\\{name} | Out-Null"
        self.canary_list = "Get-ChildItem -Force -Name C:\\Users\\Public\\drums_canary_*"
    def _u(self, d):
        return f"http://{self.ip[d]}:5000"
    def execute(self, d, command, shell=False, timeout=130):
        st, v = http("POST", self._u(d) + "/execute", {"command": command, "shell": shell}, timeout=timeout)
        return st, v
    def probe_text(self, d):
        """The Drums Windows pool's probe (Python in the guest, probe_kind python), without a canary."""
        st, v = self.execute(d, [PY, "-c", self.probe.replace("{CANARY}", "")])
        if st != 200 or not isinstance(v, dict):
            return -1, ""
        return int(v.get("returncode", -1)), (v.get("output") or "").replace("\r\n", "\n")
    def shell(self, d, cmd, timeout=130):
        st, v = self.execute(d, ["powershell", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-Command", cmd], timeout=timeout)
        if st != 200 or not isinstance(v, dict):
            return -1, ""
        return int(v.get("returncode", -1)), (v.get("output") or "").replace("\r\n", "\n")
    def up(self, n, parallel=4):
        out, lock = [], threading.Lock()
        def one(i):
            d = f"waa-{i}"
            t = time.time()
            run(["docker", "rm", "-f", d])
            sd = f"{STORE}/{i}"
            run(["rm", "-rf", sd]); os.makedirs(sd, exist_ok=True)
            rc, o, e, cps = run(["sh", "-c", f"cp --sparse=always {GOLDEN}/* {sd}/ && chmod -R u+w {sd}"], timeout=3600)
            copy_s = time.time() - t
            # the flags of WAA's scripts/run.sh (minus -it, --rm and the 8006/3389 port publications, unused here)
            rc, o, e, _ = run(["docker", "run", "-d", "--name", d, "--platform", "linux/amd64", "--device=/dev/kvm", "-e", f"RAM_SIZE={self.ram}", "-e", f"CPU_CORES={self.cpus}",
                               "-v", f"{sd}:/storage", "--cap-add", "NET_ADMIN", "--stop-timeout", "120", "--entrypoint", "/bin/bash", self.image,
                               "-c", "./entry.sh --prepare-image false --start-client false"], timeout=120)
            ip = run(["docker", "inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", d])[1].strip()
            with lock:
                self.ip[d] = ip
            ok = False
            for _ in range(900):
                st, v = http("GET", f"http://{ip}:5000/probe", timeout=3) if ip else (0, None)
                if st == 200:
                    ok = True; break
                time.sleep(1)
            with lock:
                out.append({"id": d, "ok": ok, "copy_s": copy_s, "ready_s": time.time() - t, "ip": ip, "docker_rc": rc, "err": e[:300]})
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
        s1, _ = http("POST", self._u(d) + "/setup/close_all", {}, timeout=120)
        if self.stock_sleeps:
            time.sleep(5)
        s2, _ = http("POST", self._u(d) + "/setup/clear_task_files", {}, timeout=120)
        ready = time.time() - t
        audit = self.audit_after_reset(d, ep)
        t2 = time.time()
        ok, detail = s1 == 200 and s2 == 200, f"close_all {s1} clear_task_files {s2}"
        for c in task.get("setup", []):
            rc, o = self.shell(d, c)
            if rc != 0:
                ok, detail = False, f"{c!r} exit {rc}: {o[:200]}"; break
        if self.stock_sleeps:
            time.sleep(5)
        return {"ok": ok, "ready_s": ready, "setup_s": time.time() - t2, "audit": audit, "reason": detail, "stock_sleeps_s": 10 if self.stock_sleeps else 0}
    def act(self, d, action, screenshot=True):
        t = time.time()
        if action.get("type") == "powershell":
            rc, o = self.shell(d, action["command"]); st = 200 if rc == 0 else 500
        elif action.get("type") == "pyautogui":
            st, v = self.execute(d, [PY, "-c", PREFIX + action["code"]])
        else:
            st = 400
        act_s = time.time() - t
        shot_s = nbytes = None
        if screenshot:
            t2 = time.time()
            s2, b = http("GET", self._u(d) + "/screenshot", timeout=120, raw=True)
            shot_s, nbytes = time.time() - t2, len(b or b"")
        return {"ok": st == 200, "http": st, "wall_s": time.time() - t, "act_s": act_s, "shot_s": shot_s, "shot_bytes": nbytes}
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
