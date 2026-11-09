#!/usr/bin/env python3
"""T3 analysis: reads ONE run's raw directory only and writes summary.json, tables.md and SVG figures next to it.

    python3 pool/analysis/analyze.py RAWDIR [--out DIR]

Implements the pre-registered rules (PREREGISTRATION.md): concurrency, valid rewards (+ Wilson 95 % CI), failover
(detection, no-stall, recovery R), control-plane latency, hosts. Standard library only.
"""
import argparse
import collections
import glob
import json
import math
import os
import statistics


def jl(path):
    out = []
    if not os.path.exists(path):
        return out
    for line in open(path):
        line = line.strip()
        if line:
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    return out


def wilson(k, n, z=1.96):
    if n == 0:
        return (None, None)
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return (c - h, c + h)


def pct(v, q):
    if not v:
        return None
    v = sorted(v)
    i = min(len(v) - 1, max(0, int(round(q * (len(v) - 1)))))
    return v[i]


def prom_series(path):
    """Prometheus query_range JSON -> {label-string: [(t, v)]}"""
    if not os.path.exists(path):
        return {}
    d = json.load(open(path))
    out = {}
    for r in d.get("data", {}).get("result", []):
        k = ",".join(f"{a}={b}" for a, b in sorted(r["metric"].items())) or "value"
        out[k] = [(float(t), float(v)) for t, v in r["values"] if v not in ("NaN", "+Inf", "-Inf")]
    return out


# ------------------------------------------------------------------------------------------------ svg
def svg_lines(path, title, series, xlabel, ylabel, vlines=(), hlines=(), t0=0.0, ymin=None, ymax=None, w=900, h=360):
    """series: [(name, [(x, y)])]; x in seconds (shown in minutes from t0)."""
    pts = [p for _, s in series for p in s]
    if not pts:
        return
    xs = [(x - t0) / 60 for x, _ in pts]
    ys = [y for _, y in pts]
    x0, x1 = min(xs), max(xs)
    y0 = min(ys) if ymin is None else ymin
    y1 = max(ys) if ymax is None else ymax
    for _, hv, _c in hlines:
        y0, y1 = min(y0, hv), max(y1, hv)
    if y1 == y0:
        y1 = y0 + 1
    L, R, T, B = 70, 180, 30, 45
    pw, ph = w - L - R, h - T - B
    fx = lambda x: L + (x - x0) / ((x1 - x0) or 1) * pw  # noqa: E731
    fy = lambda y: T + ph - (y - y0) / (y1 - y0) * ph  # noqa: E731
    colors = ["#2563eb", "#dc2626", "#059669", "#d97706", "#7c3aed", "#0891b2", "#be185d", "#4b5563", "#65a30d", "#9333ea", "#0f766e", "#b45309"]
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" font-family="sans-serif" font-size="11">',
         f'<rect width="{w}" height="{h}" fill="white"/>', f'<text x="{L}" y="18" font-size="13" font-weight="bold">{title}</text>',
         f'<line x1="{L}" y1="{T + ph}" x2="{L + pw}" y2="{T + ph}" stroke="#333"/>', f'<line x1="{L}" y1="{T}" x2="{L}" y2="{T + ph}" stroke="#333"/>']
    for i in range(6):
        yv = y0 + (y1 - y0) * i / 5
        o.append(f'<text x="{L - 6}" y="{fy(yv) + 4:.1f}" text-anchor="end">{yv:.4g}</text>')
        o.append(f'<line x1="{L}" y1="{fy(yv):.1f}" x2="{L + pw}" y2="{fy(yv):.1f}" stroke="#eee"/>')
        xv = x0 + (x1 - x0) * i / 5
        o.append(f'<text x="{fx(xv):.1f}" y="{T + ph + 15}" text-anchor="middle">{xv:.0f}</text>')
    o.append(f'<text x="{L + pw / 2}" y="{h - 8}" text-anchor="middle">{xlabel}</text>')
    o.append(f'<text x="14" y="{T + ph / 2}" transform="rotate(-90 14 {T + ph / 2})" text-anchor="middle">{ylabel}</text>')
    for name, xv, c in vlines:
        X = fx((xv - t0) / 60)
        o.append(f'<line x1="{X:.1f}" y1="{T}" x2="{X:.1f}" y2="{T + ph}" stroke="{c}" stroke-dasharray="4 3"/><text x="{X + 3:.1f}" y="{T + 10}" fill="{c}">{name}</text>')
    for name, yv, c in hlines:
        Y = fy(yv)
        o.append(f'<line x1="{L}" y1="{Y:.1f}" x2="{L + pw}" y2="{Y:.1f}" stroke="{c}" stroke-dasharray="2 2"/><text x="{L + pw - 2}" y="{Y - 3:.1f}" text-anchor="end" fill="{c}">{name}</text>')
    for i, (name, s) in enumerate(series):
        c = colors[i % len(colors)]
        if s:
            d = " ".join(f"{fx((x - t0) / 60):.1f},{fy(min(max(y, y0), y1)):.1f}" for x, y in s)
            o.append(f'<polyline fill="none" stroke="{c}" stroke-width="1.4" points="{d}"/>')
        o.append(f'<rect x="{L + pw + 10}" y="{T + i * 15}" width="10" height="3" fill="{c}"/><text x="{L + pw + 24}" y="{T + i * 15 + 5}">{name[:24]}</text>')
    o.append("</svg>")
    open(path, "w").write("\n".join(o))


# ------------------------------------------------------------------------------------------------ main
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("raw")
    ap.add_argument("--out", default=None)
    a = ap.parse_args()
    raw = os.path.abspath(a.raw)
    out = os.path.abspath(a.out or os.path.join(raw, "..", os.path.basename(raw) + "-analysis"))
    os.makedirs(out, exist_ok=True)
    ph = {p["phase"]: p for p in jl(os.path.join(raw, "phases.jsonl"))}
    W0, W1 = ph["trainer_start"]["window"]
    t_kill = ph.get("kill", {}).get("t")
    killed = ph.get("kill", {}).get("host")
    rows = []
    for f in sorted(glob.glob(os.path.join(raw, "trainer", "events-*.jsonl"))):
        rows += jl(f)
    S = {"raw": raw, "window": [W0, W1], "window_s": W1 - W0, "kill": ph.get("kill"), "N_rows": len(rows)}

    # rule 2: valid rewards in W
    rw = [r for r in rows if r["k"] == "reward" and W0 <= r["t"] <= W1]
    designed = [r for r in rw if r.get("expected") is not None]
    k = sum(1 for r in designed if r["valid_reward"])
    lo, hi = wilson(k, len(designed))
    bad = [r for r in designed if not r["valid_reward"]]
    S["rewards"] = {"rows_in_window": len(rw), "with_designed_score": len(designed), "valid_rewards": k,
                    "valid_rate": k / len(designed) if designed else None, "wilson95": [lo, hi], "irv": sum(1 for r in designed if r["irv"]),
                    "not_valid": len(bad), "not_valid_by_cause": collections.Counter(
                        ("checker_error" if r.get("checker_error") else "invalid_episode:" + ",".join(r.get("bad_gates") or []) if not r.get("valid") else "score_mismatch")
                        for r in bad).most_common(),
                    "oracle_rewards": sum(1 for r in designed if r["oracle"]), "oracle_failed_rows": sum(1 for r in rw if r.get("expected") is None),
                    "by_host": collections.Counter(r.get("host") for r in designed).most_common()}
    S["rewards"]["pass"] = bool(designed) and k / len(designed) >= 0.999 and S["rewards"]["irv"] == 0

    # errors in W
    er = [r for r in rows if r["k"] == "error" and W0 <= r["t"] <= W1]
    S["errors"] = {"n": len(er), "by_class_code_phase": collections.Counter((r["err"]["cls"], r["err"].get("code"), r.get("phase")) for r in er).most_common(30)}
    S["failover_continue"] = sum(1 for r in rows if r["k"] == "failover_continue")

    # rule 1: concurrency from Prometheus
    envs = prom_series(os.path.join(raw, "prom", "envs_by_status.json"))
    live = collections.defaultdict(float)
    for key, s in envs.items():
        if "status=live" in key or "status=needs_reset" in key:
            for t, v in s:
                live[t] += v
    lt = sorted((t, v) for t, v in live.items() if W0 <= t <= W1)
    # rule 3: failover
    pev = jl(os.path.join(raw, "pool-events.jsonl"))
    dead = [e for e in pev if e.get("ev") == "host_dead" and (t_kill is None or e["t"] >= t_kill - 5)]
    fo_done = [e for e in pev if e.get("ev") == "failover_done"]
    bins = collections.Counter(int((r["t"] - W0) // 60) for r in designed if r["valid_reward"])
    nb = int((W1 - W0) // 60)
    per_min = [(W0 + 60 * i + 30, bins.get(i, 0)) for i in range(nb)]
    F = {}
    R = None
    if t_kill:
        F["detection_s"] = (dead[0]["t"] - t_kill) if dead else None
        F["detection_pass"] = F["detection_s"] is not None and F["detection_s"] <= 20
        kb = int((t_kill - W0) // 60)
        F["zero_bins_after_kill"] = [i for i in range(kb, nb) if bins.get(i, 0) == 0]
        F["no_stall_pass"] = not F["zero_bins_after_kill"]
        pre = [r["t"] for r in designed if r["valid_reward"] and t_kill - 600 <= r["t"] < t_kill]
        pre_rate = len(pre) / 600.0
        F["pre_kill_valid_rewards_per_s"] = pre_rate
        vt = sorted(r["t"] for r in designed if r["valid_reward"] and r["t"] >= t_kill)
        lt_after = [(t, v) for t, v in lt if t >= t_kill]
        j = 0
        for t in range(int(t_kill) + 60, int(W1) + 1, 5):
            while j < len(vt) and vt[j] < t - 60:
                j += 1
            n = 0
            jj = j
            while jj < len(vt) and vt[jj] <= t:
                n += 1
                jj += 1
            lv = [v for tt, v in lt_after if tt <= t]
            if n / 60.0 >= 0.9 * pre_rate and lv and lv[-1] >= 2000:
                R = t - t_kill
                break
        F["recovery_s"] = R
        F["recovery_pass"] = R is not None and R <= 300
        F["min_rate_60s_after_kill"] = min((c for i, c in enumerate(bins.get(i, 0) for i in range(kb, nb))), default=None)
        lostrows = [r for r in rows if r["k"] == "error" and (r["err"].get("code") in ("episode_lost", "host_lost")) and r["t"] >= t_kill - 5]
        F["episodes_lost"] = len(lostrows)
        F["lost_by_code"] = collections.Counter(r["err"].get("code") for r in lostrows).most_common()
        F["failover_done"] = fo_done
        F["killed_host_rewards_after_kill"] = sum(1 for r in designed if r.get("host") == killed and r["t"] > t_kill + 1)
        rep = [e for e in pev if e.get("ev") in ("failover_start", "failover_done")]
        F["failover_events"] = rep
    S["failover"] = F
    outside = [(t, v) for t, v in lt if not (t_kill and t_kill <= t <= t_kill + (R if R is not None else 300))]
    S["concurrency"] = {"samples": len(lt), "median": statistics.median([v for _, v in lt]) if lt else None, "min": min((v for _, v in lt), default=None),
                        "max": max((v for _, v in lt), default=None), "min_outside_recovery": min((v for _, v in outside), default=None),
                        "samples_below_2000_outside_recovery": sum(1 for _, v in outside if v < 2000)}
    S["concurrency"]["pass"] = bool(lt) and S["concurrency"]["median"] >= 2000 and S["concurrency"]["samples_below_2000_outside_recovery"] == 0
    # every host death in the window (the planned kill and any unplanned loss such as a spot reclaim), for the labelled view
    deaths = [e for e in pev if e.get("ev") == "host_dead" and W0 <= e["t"] <= W1]
    S["host_deaths_in_window"] = [{"host": e["host"], "t_from_window_s": round(e["t"] - W0, 1), "planned": e["host"] == killed} for e in deaths]
    below = [(t, v) for t, v in lt if v < 2000]
    S["concurrency"]["samples_below_2000"] = len(below)
    S["concurrency"]["below_2000_intervals_from_window_s"] = []
    for t, v in below:
        x = round(t - W0)
        iv = S["concurrency"]["below_2000_intervals_from_window_s"]
        if iv and x - iv[-1][1] <= 20:
            iv[-1][1] = x
        else:
            iv.append([x, x])
    S["concurrency"]["min_live"] = min((v for _, v in lt), default=None)
    S["concurrency"]["time_weighted_mean"] = statistics.mean([v for _, v in lt]) if lt else None

    # rule 4: latency from the trainer (client side) and the pool (overhead)
    ops = collections.defaultdict(list)
    for r in rows:
        if r["k"] == "op" and r["ok"] and W0 <= r["t"] <= W1:
            ops[r["op"]].append(r["s"])
    S["client_latency_s"] = {op: {"n": len(v), "p50": pct(v, .5), "p95": pct(v, .95), "p99": pct(v, .99)} for op, v in sorted(ops.items())}
    lat = {}
    for f in sorted(glob.glob(os.path.join(raw, "prom", "*_p*.json"))):
        name = os.path.basename(f)[:-5]
        s = prom_series(f).get("value", [])
        sw = [v for t, v in s if W0 <= t <= W1]
        if sw:
            lat[name] = {"median_of_1m": statistics.median(sw), "max_of_1m": max(sw)}
    S["pool_latency_prom"] = lat
    # pool overhead over the whole run from the final histogram dump (exact counts)
    S["pool_overhead_final"] = overhead_from_prom_text(os.path.join(raw, "pool-metrics-final.prom"))
    # rtt by host / region
    hosts = {h["name"]: h for h in json.load(open(os.path.join(raw, "hosts.json")))} if os.path.exists(os.path.join(raw, "hosts.json")) else {}
    rt = jl(os.path.join(raw, "rtt.jsonl"))
    rr = collections.defaultdict(list)
    for r in rt:
        if W0 <= r["t"] <= W1:
            for k2, v in r.items():
                if k2 != "t" and v is not None:
                    rr[k2].append(v)
    S["rtt_healthz_s"] = {k2: {"region": hosts.get(k2, {}).get("region", "local"), "tunnel": not hosts.get(k2, {}).get("same_region", True),
                               "n": len(v), "p50": pct(v, .5), "p99": pct(v, .99)} for k2, v in sorted(rr.items())}
    by_region = collections.defaultdict(list)
    for r in rows:
        pass
    S["hosts"] = {"n": len(hosts), "serving_at_window_start": None, "regions": sorted({h["region"] for h in hosts.values()}),
                  "types": collections.Counter(h["type"] for h in hosts.values()).most_common(), "spot": all(h.get("lifecycle") == "spot" for h in hosts.values())}
    he = prom_series(os.path.join(raw, "prom", "host_envs.json"))
    serving = 0
    for key, s in he.items():
        v = [y for t, y in s if abs(t - W0) <= 30]
        if v and max(v) > 0:
            serving += 1
    S["hosts"]["serving_at_window_start"] = serving
    S["hosts"]["pass"] = serving >= 8 and len(S["hosts"]["regions"]) >= 2 and S["hosts"]["spot"]
    # memory vs CPU: per host at window middle
    sat = prom_series(os.path.join(raw, "prom", "host_saturation.json"))
    mid = (W0 + W1) / 2
    hs = {}
    for key, s in sat.items():
        lab = dict(x.split("=", 1) for x in key.split(","))
        v = [y for t, y in s if abs(t - mid) <= 60]
        if v:
            hs.setdefault(lab["host"], {})[lab["resource"]] = round(statistics.median(v), 4)
    S["saturation_mid_window"] = hs
    est = prom_series(os.path.join(raw, "prom", "env_mib_est.json"))
    S["mem_per_desktop_mib_mid_window"] = {dict(x.split("=", 1) for x in k3.split(","))["host"]: round(statistics.median([y for t, y in s if abs(t - mid) <= 60] or [0]), 1) for k3, s in est.items()}
    ups = prom_series(os.path.join(raw, "prom", "up_step_p50_by_host.json"))
    reg = collections.defaultdict(list)
    for k3, ser in ups.items():
        hn = dict(x.split("=", 1) for x in k3.split(",")).get("host")
        reg[(hosts.get(hn, {}).get("region") or ("us-east-2" if (hn or "").startswith("east2") else "us-west-2"))] += [v for t, v in ser if W0 <= t <= W1]
    S["step_upstream_p50_by_region_s"] = {r: {"median_of_1m_p50": statistics.median(v), "n": len(v)} for r, v in reg.items() if v}
    S["blocked_placements"] = {k3: sum(v for t, v in s if W0 <= t <= W1) * 10 for k3, s in prom_series(os.path.join(raw, "prom", "blocked_rate.json")).items()}
    S["pass"] = {"1_concurrency": S["concurrency"]["pass"], "2_valid_rewards": S["rewards"]["pass"], "3a_detection": F.get("detection_pass"),
                 "3b_no_stall": F.get("no_stall_pass"), "3c_recovery": F.get("recovery_pass"), "5_hosts": S["hosts"]["pass"]}
    S["pass"]["all"] = all(v for v in S["pass"].values() if v is not None) and None not in S["pass"].values()
    json.dump(S, open(os.path.join(out, "summary.json"), "w"), indent=1, default=str)

    # figures
    vl = [("kill", t_kill, "#dc2626")] if t_kill else []
    svg_lines(os.path.join(out, "fig-live-desktops.svg"), f"Live desktops through the pool (N hosts={len(hosts)})", [("live", sorted(live.items()))],
              "minutes from window start", "desktops", vlines=vl + [("window", W0, "#555"), ("end", W1, "#555")], hlines=[("2,000", 2000, "#059669")], t0=W0, ymin=0)
    svg_lines(os.path.join(out, "fig-valid-rewards-per-min.svg"), "Valid rewards per minute (all workers)", [("valid/min", per_min)],
              "minutes from window start", "valid rewards per 60 s", vlines=vl, t0=W0, ymin=0)
    ser = []
    for op in ("step", "reset", "verify", "create"):
        for q in ("p50", "p99"):
            s = prom_series(os.path.join(raw, "prom", f"req_{op}_{q}.json")).get("value", [])
            if s:
                ser.append((f"{op} {q}", s))
    svg_lines(os.path.join(out, "fig-latency.svg"), "Request latency through the pool (1 min windows)", ser, "minutes from window start", "seconds", vlines=vl, t0=W0, ymin=0)
    ser = [(n, prom_series(os.path.join(raw, "prom", f"ovh_{n.split()[0]}_{n.split()[1]}.json")).get("value", [])) for n in ("step p50", "step p99", "reset p99", "create p99")]
    svg_lines(os.path.join(out, "fig-overhead.svg"), "Control-plane overhead (pool's own time per request)", [(n, [(t, v * 1000) for t, v in s]) for n, s in ser],
              "minutes from window start", "milliseconds", vlines=vl, t0=W0, ymin=0)
    for res in ("memory", "cpu", "slots"):
        ser = []
        for key, s in sorted(sat.items()):
            lab = dict(x.split("=", 1) for x in key.split(","))
            if lab.get("resource") == res:
                ser.append((lab["host"], s))
        svg_lines(os.path.join(out, f"fig-saturation-{res}.svg"), f"Host saturation: {res}", ser, "minutes from window start", "fraction", vlines=vl, t0=W0, ymin=0, ymax=1)
    ser = [(f"{k2} ({v['region']})", [(r["t"], r[k2] * 1000) for r in rt if r.get(k2) is not None]) for k2, v in S["rtt_healthz_s"].items()]
    svg_lines(os.path.join(out, "fig-rtt.svg"), "HTTP round trip coordinator -> host (/healthz)", ser, "minutes from window start", "milliseconds", vlines=vl, t0=W0, ymin=0)
    tables(S, os.path.join(out, "tables.md"))
    print(json.dumps(S["pass"]), "->", out)


def overhead_from_prom_text(path):
    if not os.path.exists(path):
        return {}
    buckets = collections.defaultdict(list)
    for line in open(path):
        if line.startswith("pool_overhead_seconds_bucket") or line.startswith("pool_request_seconds_bucket"):
            name = line.split("{")[0].replace("_bucket", "")
            labels = line[line.index("{") + 1:line.index("}")]
            lab = dict(x.split("=", 1) for x in labels.split(","))
            op = lab["op"].strip('"')
            le = lab["le"].strip('"')
            n = float(line.rsplit(" ", 1)[1])
            buckets[(name, op)].append((float("inf") if le == "+Inf" else float(le), n))
    out = {}
    for (name, op), b in buckets.items():
        b.sort()
        tot = b[-1][1]
        if tot == 0:
            continue
        def q(p):
            for le, n in b:
                if n >= p * tot:
                    return le
        out[f"{name}:{op}"] = {"n": int(tot), "p50_le": q(.5), "p99_le": q(.99), "p999_le": q(.999)}
    return out


def tables(S, path):
    r, c, f = S["rewards"], S["concurrency"], S["failover"]
    L = ["| rule | metric | value | pass |", "|---|---|---|---|",
         f"| 1 | live desktops in W (median / min outside recovery; samples) | {c['median']} / {c['min_outside_recovery']} ({c['samples']}) | {c['pass']} |",
         f"| 2 | valid rewards / rewards with a designed score | {r['valid_rewards']} / {r['with_designed_score']} = {r['valid_rate']:.5f} (95 % CI {r['wilson95'][0]:.5f}-{r['wilson95'][1]:.5f}); IRV {r['irv']} | {r['pass']} |" if r["valid_rate"] is not None else "| 2 | no rewards | | False |",
         f"| 3a | host-dead detection after the kill | {f.get('detection_s')} s | {f.get('detection_pass')} |",
         f"| 3b | 60 s bins with 0 valid rewards after the kill | {len(f.get('zero_bins_after_kill') or [])} | {f.get('no_stall_pass')} |",
         f"| 3c | recovery R (rate >= 90 % of pre-kill and live >= 2,000) | {f.get('recovery_s')} s | {f.get('recovery_pass')} |",
         f"| 5 | hosts serving at window start / regions / all spot | {S['hosts']['serving_at_window_start']} / {S['hosts']['regions']} / {S['hosts']['spot']} | {S['hosts']['pass']} |"]
    L += ["", "| op | n | client p50 s | p95 | p99 |", "|---|---|---|---|---|"]
    for op, v in S["client_latency_s"].items():
        L.append(f"| {op} | {v['n']} | {v['p50']} | {v['p95']} | {v['p99']} |")
    L += ["", "| pool histogram | n | p50 <= s | p99 <= s | p99.9 <= s |", "|---|---|---|---|---|"]
    for k, v in sorted(S["pool_overhead_final"].items()):
        L.append(f"| {k} | {v['n']} | {v['p50_le']} | {v['p99_le']} | {v['p999_le']} |")
    L += ["", "| host | region | tunnel | /healthz RTT p50 s | p99 s |", "|---|---|---|---|---|"]
    for k, v in S["rtt_healthz_s"].items():
        L.append(f"| {k} | {v['region']} | {v['tunnel']} | {v['p50']} | {v['p99']} |")
    open(path, "w").write("\n".join(L) + "\n")


if __name__ == "__main__":
    main()
