#!/usr/bin/env python3
"""bench.py URL TOKEN MODE OUT.json [TASK] [N_DENSITY] [CYCLES]
Measures, against one drumsd (bare host or inside a tenant VM), the two numbers the isolation cost is made of:
  1. fork/reset latency: create 1 env (optionally with an app task), fork 8 children, reset 1, CYCLES times.
  2. density: create idle envs until Overloaded (429) or N_DENSITY, record idle memory/env and the max reached.
Everything comes from the SDK (what a customer sees). Raw per-op rows go in the JSON so the report reads only raw/."""
import json, sys, time, statistics
import drums

def pct(xs, p):
    if not xs: return None
    xs = sorted(xs); k = max(0, min(len(xs)-1, int(round((p/100.0)*(len(xs)-1)))))
    return round(xs[k], 3)

def summ(xs):
    return dict(n=len(xs), p50=pct(xs,50), p95=pct(xs,95), mean=round(statistics.mean(xs),3) if xs else None,
                min=round(min(xs),3) if xs else None, max=round(max(xs),3) if xs else None)

def forkreset(c, task, cycles):
    forks, resets, valid, invalid, rows = [], [], 0, 0, []
    e = c.create(task=task) if task else c.create()
    e.wait_valid(timeout=90)
    for i in range(cycles):
        t0 = time.time()
        fr = e.fork(8, strict=False, keep_state=False)
        dt = time.time() - t0
        forks.append(dt)
        kids = fr.ok if hasattr(fr, "ok") else list(fr)
        for k in kids:
            try:
                ok = k.valid(timeout=30)
                valid += 1 if ok else 0; invalid += 0 if ok else 1
            except Exception:
                invalid += 1
            finally:
                try: k.close()
                except Exception: pass
        t0 = time.time(); e.reset(); rdt = time.time() - t0; resets.append(rdt)
        rows.append(dict(cycle=i, fork_s=round(dt,3), reset_s=round(rdt,3), kids=len(kids)))
    try: e.close()
    except Exception: pass
    return dict(fork=summ(forks), reset=summ(resets), children_valid=valid, children_invalid=invalid, rows=rows)

def density(c, task, cap):
    envs, rows = [], []
    h0 = c.health()
    try:
        while len(envs) < cap:
            t0 = time.time()
            try:
                e = c.create(task=task) if task else c.create()
            except drums.Overloaded as x:
                rows.append(dict(i=len(envs), overloaded=str(x))); break
            except Exception as x:
                rows.append(dict(i=len(envs), error=str(x))); break
            envs.append(e)
            h = c.health()
            rows.append(dict(i=len(envs), create_s=round(time.time()-t0,3),
                             mem_avail=h.get("admission",{}).get("available_bytes") or h.get("mem_available_bytes"),
                             state_free=h.get("state_dir_free_bytes")))
    finally:
        hN = c.health()
        for e in envs:
            try: e.close()
            except Exception: pass
    # idle memory/env estimate from MemAvailable drop
    a0 = (h0.get("admission",{}) or {}).get("available_bytes") or h0.get("mem_available_bytes")
    aN = (hN.get("admission",{}) or {}).get("available_bytes") or hN.get("mem_available_bytes")
    per = round((a0-aN)/len(envs)/1e6,1) if (a0 and aN and envs) else None
    return dict(max_envs=len(envs), mem_avail_start=a0, mem_avail_end=aN, mem_mb_per_env_est=per, rows=rows)

def main():
    url, token, mode, out = sys.argv[1:5]
    task = sys.argv[5] if len(sys.argv) > 5 and sys.argv[5] != "-" else None
    ncap = int(sys.argv[6]) if len(sys.argv) > 6 else 40
    cycles = int(sys.argv[7]) if len(sys.argv) > 7 else 10
    c = drums.connect(url=url, token=token)
    c.reset_metrics()
    rep = dict(mode=mode, when=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()), url=url, task=task,
               host_health=c.health())
    print(f"[{mode}] fork/reset x{cycles} ...", flush=True)
    rep["forkreset"] = forkreset(c, task, cycles)
    print(f"[{mode}] density up to {ncap} ...", flush=True)
    rep["density"] = density(c, None, ncap)   # density uses blank desktops (idle footprint)
    rep["metrics"] = c.metrics()
    with open(out, "w") as f: json.dump(rep, f, indent=2)
    fr = rep["forkreset"]
    print(json.dumps(dict(mode=mode, fork_p50=fr["fork"]["p50"], fork_p95=fr["fork"]["p95"],
                          reset_p50=fr["reset"]["p50"], reset_p95=fr["reset"]["p95"],
                          kids_valid=fr["children_valid"], kids_invalid=fr["children_invalid"],
                          max_envs=rep["density"]["max_envs"], mb_per_env=rep["density"]["mem_mb_per_env_est"])))
    try: c.close()
    except Exception: pass

if __name__ == "__main__":
    main()
