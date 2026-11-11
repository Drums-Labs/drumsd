#!/usr/bin/env python3
"""Regenerate every T5 table from raw soak files (raw/*.jsonl). Usage: analyze.py RAW_DIR [--md OUT.md] [--json OUT.json]

Per soak file (one arm at one N): reset p50/p99 (bootstrap 95% CI, 2,000 resamples, seed 0), latency-step p50/p99,
valid-reward rate and clean-start rate (Wilson 95%), checker agreement, episodes, desktops.
Definitions: PLAN.md. Reset for Drums = start.ready_s (drumsd cost.ready_s); for incumbents = start.ready_s (their
stock reset calls incl. their own sleeps); `reset_nosleep` subtracts start.stock_sleeps_s where present.
"""
import json, math, os, random, sys

def pct(xs, q):
    if not xs:
        return None
    s = sorted(xs)
    k = (len(s) - 1) * q
    f, c = math.floor(k), math.ceil(k)
    return s[f] if f == c else s[f] + (s[c] - s[f]) * (k - f)

def boot_ci(xs, q, n=2000, seed=0):
    if len(xs) < 2:
        return (None, None)
    r = random.Random(seed)
    v = sorted(pct([r.choice(xs) for _ in xs], q) for _ in range(n))
    return (v[int(0.025 * n)], v[int(0.975 * n) - 1])

def wilson(k, n, z=1.96):
    if n == 0:
        return (None, None, None)
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return (p, max(0, c - h), min(1, c + h))

def summarize(path):
    rows = [json.loads(l) for l in open(path)]
    meta = next((r for r in rows if r.get("kind") == "meta"), {})
    eps = [r for r in rows if r.get("kind") == "episode"]
    ready = next((r for r in rows if r.get("kind") == "ready"), {})
    health = next((r for r in rows if r.get("kind") == "health"), {})
    done = next((r for r in rows if r.get("kind") == "done"), {})
    resets = [e["start"]["ready_s"] for e in eps if e.get("start", {}).get("ok") and e["start"].get("ready_s") is not None]
    nosleep = [e["start"]["ready_s"] - e["start"].get("stock_sleeps_s", 0) / 2 for e in eps if e.get("start", {}).get("ok") and e["start"].get("ready_s") is not None]
    lat = [s["wall_s"] for e in eps for s in e.get("steps", []) if s.get("kind") == "latency" and s.get("ok")]
    shots = [s["shot_s"] for e in eps for s in e.get("steps", []) if s.get("kind") == "latency" and s.get("ok") and s.get("shot_s") is not None]
    valid = sum(1 for e in eps if e.get("valid"))
    agree = sum(1 for e in eps if e.get("agrees"))
    if meta.get("arm") == "drums":
        clean = [all(g.get("ok") for g in (e.get("start", {}).get("gates") or []) if g["name"] == "state_hash") and any(g["name"] == "state_hash" for g in (e.get("start", {}).get("gates") or [])) for e in eps if e.get("start")]
    else:
        clean = [bool((e.get("start", {}).get("audit") or {}).get("state_hash_ok")) for e in eps if e.get("start")]
    out = {"file": os.path.basename(path), "arm": meta.get("arm"), "os": meta.get("os"), "image": meta.get("image"), "n": meta.get("n"),
           "desktops_ready": len(ready.get("desktops", [])), "episodes": len(eps), "soak_s": done.get("soak_s"),
           "reset_p50": pct(resets, .5), "reset_p50_ci": boot_ci(resets, .5), "reset_p99": pct(resets, .99), "reset_p99_ci": boot_ci(resets, .99),
           "reset_nosleep_p50": pct(nosleep, .5) if meta.get("arm") == "waa" else None,
           "step_p50": pct(lat, .5), "step_p50_ci": boot_ci(lat, .5), "step_p99": pct(lat, .99), "step_p99_ci": boot_ci(lat, .99),
           "shot_p50": pct(shots, .5),
           "valid_rate": wilson(valid, len(eps)), "valid": valid, "agree": agree, "agree_rate": wilson(agree, len(eps)),
           "clean_start_rate": wilson(sum(clean), len(clean)), "clean_starts": sum(clean),
           "invalid_reasons": sorted({((e.get("start") or {}).get("reason") or e.get("error") or "verify/steps")[:120] for e in eps if not e.get("valid")})[:10],
           "host_mem_available_mib_end": ((health.get("health") or {}).get("host") or {}).get("mem_available_mib")}
    if eps and done.get("soak_s"):
        out["episodes_per_hour"] = len(eps) / done["soak_s"] * 3600
    hosts = [r for r in rows if r.get("kind") == "host"]
    out["mem_available_min_pct"] = min((h["mem_available_mib"] / h["mem_total_mib"] * 100 for h in hosts), default=None)
    out["load1_max"] = max((h["load1"] for h in hosts), default=None)
    out["oom_kills"] = done.get("oom_kills")
    per_desk = {}
    for e in eps:
        per_desk[e["desktop"]] = per_desk.get(e["desktop"], 0) + 1
    # density rung criteria (PLAN.md): all N ready; >= 10 min and >= 2 episodes per desktop; valid >= 99%;
    # latency-step p99 <= 10 s; no OOM kill; MemAvailable >= 2% throughout
    crit = {"all_ready": out["desktops_ready"] == out["n"],
            "duration": (done.get("soak_s") or 0) >= 600 and len(per_desk) == out["n"] and min(per_desk.values(), default=0) >= 2,
            "valid_99": out["valid_rate"][0] is not None and out["valid_rate"][0] >= 0.99,
            "step_p99_10s": out["step_p99"] is not None and out["step_p99"] <= 10.0,
            "no_oom": not out["oom_kills"],
            "mem_2pct": out["mem_available_min_pct"] is None or out["mem_available_min_pct"] >= 2.0}
    out["density_criteria"] = crit
    out["density_pass"] = all(crit.values())
    return out

def fmt(x, d=2):
    return "-" if x is None else (f"{x:.{d}f}" if isinstance(x, float) else str(x))

def main():
    raw = sys.argv[1]
    files = sorted(f for f in os.listdir(raw) if f.endswith(".jsonl"))
    res = [summarize(os.path.join(raw, f)) for f in files]
    if "--json" in sys.argv:
        json.dump(res, open(sys.argv[sys.argv.index("--json") + 1], "w"), indent=1)
    lines = ["| file | arm | image | N | ready | episodes | reset p50 s (95% CI) | reset p99 s | step p50 s | step p99 s | valid | clean start | agree |", "|" + "---|" * 13]
    for r in res:
        lines.append(f"| {r['file']} | {r['arm']} | {r['image']} | {r['n']} | {r['desktops_ready']} | {r['episodes']} | {fmt(r['reset_p50'])} ({fmt(r['reset_p50_ci'][0])}-{fmt(r['reset_p50_ci'][1])}) | {fmt(r['reset_p99'])} | "
                     f"{fmt(r['step_p50'])} | {fmt(r['step_p99'])} | {r['valid']}/{r['episodes']} ({fmt(r['valid_rate'][0]*100 if r['valid_rate'][0] is not None else None,1)}%) | {r['clean_starts']}/{r['episodes']} | {r['agree']}/{r['episodes']} |")
    lines += ["", "| file | N | ready | soak s | min eps/desktop ok | valid | step p99 s | OOM kills | min MemAvailable % | max load1 | rung passes |", "|" + "---|" * 11]
    for r in res:
        c = r["density_criteria"]
        lines.append(f"| {r['file']} | {r['n']} | {r['desktops_ready']} | {fmt(r['soak_s'],0)} | {c['duration']} | {fmt(r['valid_rate'][0]*100 if r['valid_rate'][0] is not None else None,1)}% | {fmt(r['step_p99'])} | {r['oom_kills']} | {fmt(r['mem_available_min_pct'],1)} | {fmt(r['load1_max'],0)} | {r['density_pass']} |")
    md = "\n".join(lines)
    if "--md" in sys.argv:
        open(sys.argv[sys.argv.index("--md") + 1], "w").write(md + "\n")
    print(md)

if __name__ == "__main__":
    main()
