#!/usr/bin/env python3
"""Episode soak for one arm at N concurrent desktops. Writes one JSON line per episode (raw data; analyze.py
derives every table from these files).

Episode = start (reset to the arm's start state + the task's setup) -> one latency step (a harmless GUI action with
a screenshot) -> the oracle's actions (mode oracle) or nothing (mode noop) -> verify. In mode mixed, passes over
the task list alternate oracle/noop so every checker is seen passing and failing. Task order is deterministic: desktop i,
episode k runs task (i + k) mod T (sorted ids), so arms see the same task sequence.
"""
import argparse, json, os, subprocess, sys, threading, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from arms import DrumsArm, AwArm, http
from checker import load_tasks

LAT_STEP = {"android": {"action_type": "click", "x": 540, "y": 30},
            "windows": {"type": "pyautogui", "code": "pyautogui.moveTo(700, 450)"}}

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--arm", required=True, choices=["drums", "aw", "waa"])
    ap.add_argument("--os", required=True, choices=["android", "windows"])
    ap.add_argument("--image", required=True)
    ap.add_argument("--url", default="http://127.0.0.1:7070")
    ap.add_argument("--token-file", default="/etc/drumsd-os.token")
    ap.add_argument("--pool-config", help="drumsd-os config (probe and probe_exclude for incumbent audits)")
    ap.add_argument("--pool", help="pool name in --pool-config whose probe to use")
    ap.add_argument("--n", type=int, required=True)
    ap.add_argument("--episodes", type=int, required=True, help="episodes per desktop")
    ap.add_argument("--tasks", required=True)
    ap.add_argument("--only", default="", help="comma list of task ids")
    ap.add_argument("--mode", default="mixed", choices=["mixed", "oracle", "noop"])
    ap.add_argument("--out", required=True)
    ap.add_argument("--keep", action="store_true", help="do not destroy desktops at the end")
    ap.add_argument("--reuse", default="", help="comma list of existing desktop ids (skip up)")
    ap.add_argument("--apks", default="")
    ap.add_argument("--configure", default="", help="file with configure shell commands (incumbent arms)")
    ap.add_argument("--min-seconds", type=float, default=0, help="keep running episodes until this long AND --episodes reached")
    a = ap.parse_args()

    tasks = load_tasks(a.tasks)
    ids = sorted(tasks)
    if a.only:
        ids = [i for i in ids if i in a.only.split(",")]
    os.makedirs(os.path.dirname(os.path.abspath(a.out)), exist_ok=True)
    out = open(a.out, "a")
    lock = threading.Lock()
    def rec(v):
        v["t"] = time.time()
        with lock:
            out.write(json.dumps(v) + "\n"); out.flush()
        try:
            os.utime("/run/drums-busy")
        except Exception:
            pass

    probe = None
    if a.pool_config:
        cfg = json.load(open(a.pool_config))
        p = [p for p in cfg["pools"] if p["name"] == a.pool][0]
        probe = p["probe"].replace("{EXCLUDE}", p.get("probe_exclude", "^$"))
        if p.get("probe_kind") == "python" and a.arm != "waa":
            raise SystemExit("a python probe is only supported for the waa arm")
    if a.arm == "drums":
        arm = DrumsArm(a.url, open(a.token_file).read().strip(), a.image)
    elif a.arm == "aw":
        conf = [l.strip() for l in open(a.configure)] if a.configure else []
        arm = AwArm(a.image, probe, apks=[x for x in a.apks.split(",") if x], configure_cmds=[c for c in conf if c and not c.startswith("#")])
    else:
        from waa_arm import WaaArm
        arm = WaaArm(a.image, probe)

    meta = {"kind": "meta", "argv": sys.argv, "arm": a.arm, "os": a.os, "image": a.image, "n": a.n, "episodes": a.episodes, "mode": a.mode, "tasks": ids}
    rec(meta)
    t0 = time.time()
    if a.reuse:
        arm.ids = a.reuse.split(",")
    else:
        ups = arm.up(a.n)
        for u in ups:
            rec({"kind": "up", **({"view": None} if False else {}), "detail": u})
    rec({"kind": "ready", "desktops": arm.ids, "up_s": time.time() - t0})
    if len(arm.ids) < a.n:
        rec({"kind": "error", "error": f"only {len(arm.ids)} of {a.n} desktops came up"})

    stop_sampler = threading.Event()
    def sampler():
        while not stop_sampler.is_set():
            mi = {l.split(":")[0]: int(l.split()[1]) for l in open("/proc/meminfo") if l.split(":")[0] in ("MemTotal", "MemAvailable", "SwapFree")}
            ksm = None
            try:
                ksm = int(open("/sys/kernel/mm/ksm/pages_sharing").read())
            except Exception:
                pass
            rec({"kind": "host", "mem_total_mib": mi["MemTotal"] // 1024, "mem_available_mib": mi["MemAvailable"] // 1024, "load1": float(open("/proc/loadavg").read().split()[0]), "ksm_pages_sharing": ksm})
            stop_sampler.wait(10)
    def oom_count():
        try:
            return subprocess.run(["sudo", "-n", "dmesg"], capture_output=True, text=True, timeout=30).stdout.count("Out of memory")
        except Exception:
            return None
    oom0 = oom_count()
    threading.Thread(target=sampler, daemon=True).start()

    def worker(i, d):
        k = -1
        while True:
            k += 1
            if k >= a.episodes and time.time() - t1 >= a.min_seconds:
                break
            task = tasks[ids[(i + k) % len(ids)]]
            # mixed: passes over the task list alternate oracle / no-op, so each task is seen passing and failing
            oracle = a.mode == "oracle" or (a.mode == "mixed" and (k // len(ids)) % 2 == 0)
            ep = f"{i}-{k}"
            e = {"kind": "episode", "arm": a.arm, "desktop": d, "i": i, "k": k, "task": task["id"], "oracle": oracle, "expect": 1.0 if oracle else 0.0}
            try:
                s = arm.start_episode(d, task) if a.arm == "drums" else arm.start_episode(d, task, ep)
                e["start"] = s
                steps = []
                if s["ok"]:
                    steps.append({"kind": "latency", **arm.act(d, LAT_STEP[a.os], screenshot=True)})
                    if oracle:
                        for act in task["oracle"]:
                            steps.append({"kind": "oracle", "action": act, **arm.act(d, act, screenshot=False)})
                    e["steps"] = steps
                    v = arm.verify(d, task) if a.arm == "drums" else arm.verify(d, task, ep)
                    e["verify"] = v
                    steps_ok = all(x["ok"] for x in steps)
                    if a.arm == "drums":
                        e["valid"] = bool(v.get("ok") and v.get("valid") and steps_ok)
                    else:
                        e["valid"] = bool(v.get("ok") and v.get("isolation_end") and steps_ok)
                    e["reward"] = v.get("reward")
                    e["agrees"] = e["reward"] == e["expect"]
                else:
                    e["valid"], e["reward"], e["agrees"] = False, None, False
            except Exception as ex:
                e["valid"], e["error"] = False, repr(ex)
            rec(e)

    ths = [threading.Thread(target=worker, args=(i, d)) for i, d in enumerate(arm.ids)]
    t1 = time.time()
    [t.start() for t in ths]
    [t.join() for t in ths]
    stop_sampler.set()
    rec({"kind": "done", "soak_s": time.time() - t1, "oom_kills": (oom_count() - oom0) if oom0 is not None else None})
    if a.arm == "drums":
        st, h = http("GET", a.url + "/v1/health", token=arm.token)
        rec({"kind": "health", "health": h})
    if not a.keep:
        if a.arm == "drums":
            arm.down()
        else:
            logdir = os.path.splitext(os.path.abspath(a.out))[0] + ".logs"
            os.makedirs(logdir, exist_ok=True)
            arm.down(logdir)

if __name__ == "__main__":
    main()
