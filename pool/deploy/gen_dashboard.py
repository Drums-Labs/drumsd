#!/usr/bin/env python3
"""Writes drums-pool-dashboard.json (Grafana): the T3 SLO dashboard over drums-pool's Prometheus metrics."""
import json
DS = {"type": "prometheus", "uid": "prom"}
def q(expr, legend):
    return {"datasource": DS, "expr": expr, "legendFormat": legend, "refId": legend[:20] or "A"}
def ts(title, targets, unit="short", x=0, y=0, w=12, h=8, thresholds=None):
    p = {"type": "timeseries", "title": title, "datasource": DS, "gridPos": {"x": x, "y": y, "w": w, "h": h}, "targets": targets,
         "fieldConfig": {"defaults": {"unit": unit}, "overrides": []}, "options": {"legend": {"displayMode": "table", "placement": "right", "calcs": ["lastNotNull", "max"]}}}
    if thresholds:
        p["fieldConfig"]["defaults"]["thresholds"] = {"mode": "absolute", "steps": [{"color": "green", "value": None}] + [{"color": c, "value": v} for v, c in thresholds]}
        p["fieldConfig"]["defaults"]["custom"] = {"thresholdsStyle": {"mode": "line"}}
    return p
def stat(title, expr, unit, x, y, w=4, h=4, dec=None):
    p = {"type": "stat", "title": title, "datasource": DS, "gridPos": {"x": x, "y": y, "w": w, "h": h}, "targets": [q(expr, title)],
         "fieldConfig": {"defaults": {"unit": unit}}, "options": {"reduceOptions": {"calcs": ["lastNotNull"]}}}
    if dec is not None:
        p["fieldConfig"]["defaults"]["decimals"] = dec
    return p
VALID = 'sum(rate(pool_verify_total{valid="true",checker_error="false"}[5m])) / sum(rate(pool_verify_total[5m]))'
def pct(op, quant, metric="pool_request_seconds"):
    return f'histogram_quantile({quant}, sum by (le) (rate({metric}_bucket{{op="{op}"}}[1m])))'
panels = [
    stat("Live desktops", 'sum(pool_envs{status="live"})', "short", 0, 0),
    stat("Valid-reward rate (5 min)", VALID, "percentunit", 4, 0, dec=3),
    stat("Healthy hosts", "sum(pool_host_up)", "short", 8, 0),
    stat("Create queue", "pool_queue_depth", "short", 12, 0),
    stat("Episodes lost (host failures)", "sum(pool_episodes_lost_total)", "short", 16, 0),
    stat("Step overhead p99 (pool only)", pct("step", 0.99, "pool_overhead_seconds"), "s", 20, 0, dec=4),
    ts("Desktops by status / per host", [q("sum by (status) (pool_envs)", "{{status}}"), q("pool_host_envs", "{{host}}")], "short", 0, 4),
    ts("Valid-reward rate (SLO 99.9 %)", [q(VALID, "valid rate 5m")], "percentunit", 12, 4, thresholds=[(0.999, "red")]),
    ts("Step latency through the pool", [q(pct("step", 0.5), "p50"), q(pct("step", 0.95), "p95"), q(pct("step", 0.99), "p99")], "s", 0, 12, w=8),
    ts("Reset latency", [q(pct("reset", 0.5), "p50"), q(pct("reset", 0.95), "p95"), q(pct("reset", 0.99), "p99")], "s", 8, 12, w=8),
    ts("Create latency (incl. queue) and fork", [q(pct("create", 0.5), "create p50"), q(pct("create", 0.99), "create p99"), q(pct("branch", 0.5), "fork p50"), q(pct("branch", 0.99), "fork p99")], "s", 16, 12, w=8),
    ts("Verify (reward) latency", [q(pct("verify", 0.5), "p50"), q(pct("verify", 0.99), "p99")], "s", 0, 20, w=8),
    ts("Control-plane overhead (pool's own time)", [q(pct("step", 0.5, "pool_overhead_seconds"), "step p50"), q(pct("step", 0.99, "pool_overhead_seconds"), "step p99"),
        q(pct("create", 0.99, "pool_overhead_seconds"), "create p99")], "s", 8, 20, w=8),
    ts("Step upstream latency by host (inter-region)", [q('histogram_quantile(0.5, sum by (le, host) (rate(pool_upstream_seconds_bucket{op="step"}[1m])))', "{{host}}")], "s", 16, 20, w=8),
    ts("Host saturation: memory", [q('pool_host_saturation{resource="memory"}', "{{host}}")], "percentunit", 0, 28, w=8, thresholds=[(0.95, "red")]),
    ts("Host saturation: CPU (load1 / cpus x limit)", [q('pool_host_saturation{resource="cpu"}', "{{host}}")], "percentunit", 8, 28, w=8, thresholds=[(0.95, "red")]),
    ts("Host saturation: slots", [q('pool_host_saturation{resource="slots"}', "{{host}}")], "percentunit", 16, 28, w=8),
    ts("Slot health: host state", [q("sum by (state) (pool_host_state)", "{{state}}")], "short", 0, 36, w=8),
    ts("Memory per desktop (learned) and MemAvailable", [q("pool_host_env_mem_mib_estimate", "{{host}} MiB/desktop")], "short", 8, 36, w=8),
    ts("Placement blocked by constraint (rate)", [q("sum by (reason) (rate(pool_place_blocked_total[1m]))", "{{reason}}")], "short", 16, 36, w=8),
    ts("Queue wait and refusals absorbed", [q('histogram_quantile(0.99, sum by (le) (rate(pool_queue_wait_seconds_bucket[1m])))', "queue wait p99"),
        q("sum(rate(pool_host_refused_total[1m]))", "host refusals/s (absorbed)"), q("sum(rate(pool_queue_timeout_total[1m]))", "queue timeouts/s")], "short", 0, 44, w=8),
    ts("Failover", [q("sum by (outcome) (increase(pool_failover_total[1m]))", "{{outcome}}"), q("sum(increase(pool_episodes_lost_total[1m]))", "episodes lost")], "short", 8, 44, w=8),
    ts("Health poll RTT by host (inter-region)", [q("pool_host_health_rtt_seconds", "{{host}} ({{region}})")], "s", 16, 44, w=8),
    ts("Request rate by op", [q("sum by (op) (rate(pool_requests_total[1m]))", "{{op}}")], "reqps", 0, 52, w=12),
    ts("Errors by op and code", [q('sum by (op, code) (rate(pool_requests_total{code!~"2.."}[1m]))', "{{op}} {{code}}")], "reqps", 12, 52, w=12),
]
d = {"title": "drums-pool SLOs", "uid": "drums-pool-slo", "schemaVersion": 39, "time": {"from": "now-2h", "to": "now"}, "refresh": "10s", "panels": panels}
json.dump(d, open("dashboards/drums-pool-dashboard.json", "w"), indent=1)
print(len(panels), "panels")
