"""N concurrent envs through the SDK: each is created with a task (alternating over --tasks), waits for its gates, takes
--steps scripted steps with screenshots, verifies, resets once, verifies again and closes. Every failure is counted with
its type; nothing is retried by this script.

    python examples/concurrency.py --n 20 --tasks 035f41ba,2ad9387a [--out conc.json]
"""
from __future__ import annotations

import argparse
import json
import os
import threading
import time

import drums


def episode(client, i, task, steps):
    rec = {"i": i, "task": task, "phases": {}}
    t = time.monotonic()
    e = client.create(image="osworld-ubuntu", task=task)
    rec["env"] = e.id
    rec["phases"]["create_s"] = round(time.monotonic() - t, 3)
    try:
        t = time.monotonic()
        e.wait_valid()
        rec["phases"]["gates_s"] = round(time.monotonic() - t, 3)
        t = time.monotonic()
        for k in range(steps):
            r = e.step(f"pyautogui.moveTo({200 + 40 * k}, {300 + 20 * k}); time.sleep(0.2)", screenshot=True)
            assert r.observation and r.observation.width
        rec["phases"]["steps_s"] = round(time.monotonic() - t, 3)
        r1 = e.verify(strict=False)
        rec["reward1"] = {"score": r1.score, "valid": r1.valid, "checker_error": r1.checker_error}
        t = time.monotonic()
        e.reset()
        rec["phases"]["reset_s"] = round(time.monotonic() - t, 3)
        r2 = e.verify(strict=False)
        rec["reward2"] = {"score": r2.score, "valid": r2.valid, "checker_error": r2.checker_error}
        rec["ok"] = r1.valid and r2.valid and r1.checker_error is None and r2.checker_error is None
    finally:
        rec["metrics"] = e.metrics()["usage"]
        e.close()
    return rec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=os.environ.get("DRUMS_URL", "http://127.0.0.1:7070"))
    ap.add_argument("--token", default=os.environ.get("DRUMS_TOKEN"))
    ap.add_argument("--n", type=int, default=20)
    ap.add_argument("--steps", type=int, default=5)
    ap.add_argument("--tasks", default="035f41ba,2ad9387a")
    ap.add_argument("--out", default="concurrency.json")
    a = ap.parse_args()
    c = drums.connect(a.url, a.token)
    c.reset_metrics()
    tasks = [c.task(t) for t in a.tasks.split(",")]
    samples, stop = [], threading.Event()

    def sampler():                        # host memory and load while the envs run (from /v1/health)
        while not stop.is_set():
            try:
                h = c.health()
                samples.append({"t": round(time.monotonic() - t0, 1), "mem_available_gib": round((h.get("host_mem_available_bytes") or 0) / 2**30, 2),
                                "loadavg": h.get("loadavg"), "slots": h.get("slots_by_state")})
            except drums.DrumsError:
                pass
            stop.wait(2)
    t0 = time.monotonic()
    th = threading.Thread(target=sampler, daemon=True)
    th.start()
    res = drums.Client.map(lambda i: episode(c, i, tasks[i % len(tasks)], a.steps), range(a.n), workers=a.n)
    wall = time.monotonic() - t0
    stop.set(); th.join()
    rows, fails = [], {}
    for i, r in enumerate(res):
        if isinstance(r, Exception):
            k = type(r).__name__
            fails[k] = fails.get(k, 0) + 1
            rows.append({"i": i, "error": f"{k}: {r}"[:800]})
            print(f"[{i:2}] FAILED {k}: {str(r)[:300]}", flush=True)
        else:
            rows.append(r)
            if not r["ok"]:
                fails["invalid_or_checker_error"] = fails.get("invalid_or_checker_error", 0) + 1
            print(f"[{i:2}] {r['env']:>6} {r['task']['id'][:8]} ok={r['ok']} {r['phases']} r1={r['reward1']['score']} r2={r['reward2']['score']}", flush=True)
    ok = sum(1 for r in rows if r.get("ok"))
    m = c.metrics()
    print(f"\nN={a.n} concurrent: {ok}/{a.n} episodes ran end to end valid; failures by type {fails}; wall {wall:.1f} s")
    for k, v in sorted(m["usage"]["ops"].items()):
        print(f"  {k:<11} n={v['n']:<4} failed={v['failed']:<3} p50={v['p50_s']}s p95={v['p95_s']}s")
    print(f"  episodes {m['usage']['episodes']} failures {m['usage']['failures_by_cause']} retries {m['usage']['retries_by_cause']}")
    print(f"  cost {m['cost']}")
    if samples:
        lo = min(samples, key=lambda x: x["mem_available_gib"])
        print(f"  host: lowest MemAvailable {lo['mem_available_gib']} GiB at t={lo['t']} s (load {lo['loadavg']}); {len(samples)} samples")
    json.dump({"n": a.n, "ok": ok, "failures": fails, "wall_s": wall, "rows": rows, "metrics": m, "host_samples": samples}, open(a.out, "w"), indent=1, default=str)


if __name__ == "__main__":
    main()
