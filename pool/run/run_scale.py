#!/usr/bin/env python3
"""T3 scale test, run on the coordinator: bring up the multi-host pool over the launched hosts and run the
pre-registered measurement. Standard library only (plus the drums SDK for the trainer).

    python3 pool/run/run_scale.py --hosts hosts.json --out RAWDIR --workers 2100 --measure-s 3600 \
        --warmup-s 900 --kill-at-s 1800 --kill-region us-east-2 [--warm-per-host 4] [--dry-run-minutes 0]

hosts.json: the `fleet.py status` output (a list of {id, region, private_ip, public_ip, role, type, vcpus}).

Phases (each writes RAWDIR/phases.jsonl with UTC times):
 1. hosts: wait for ssh and first boot; read each host's token (kept in /etc/drums-pool/hosts, 0600, never printed);
    hosts in the coordinator's region are reached on their private IP, others through one ssh tunnel each
    (drumsd listens on the host, the tunnel is encrypted; no drumsd port is open to the internet).
 2. pool + Prometheus + Alertmanager + Grafana + alert log + host samplers + latency probes.
 3. trainer: replay_trainer.py with --workers W for warmup + measure seconds. The measurement window is
    [start + warmup, start + warmup + measure].
 4. kill: at window start + kill-at-s, the host in --kill-region with the most envs is powered off at once
    (sysrq 'o': no shutdown, no goodbye; it terminates like a reclaimed spot instance).
 5. collect: pool events, trainer events, snapshots, Prometheus range queries + TSDB snapshot, host samplers, drumsd
    metrics, alert notifications; SHA256SUMS.
"""
import argparse
import json
import os
import shlex
import socket
import subprocess
import sys
import threading
import time
import urllib.parse
import urllib.request

REPO = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
O = "/opt/drums-pool"
KEY = os.path.expanduser("~/.ssh/t3-coord")
SSH = ["ssh", "-i", KEY, "-o", "StrictHostKeyChecking=accept-new", "-o", "UserKnownHostsFile=" + os.path.expanduser("~/.ssh/t3_known_hosts"),
       "-o", "ConnectTimeout=10", "-o", "ServerAliveInterval=10", "-o", "ServerAliveCountMax=3", "-o", "BatchMode=yes"]


def now():
    return time.time()


def utc(t=None):
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime(t or now()))


class Run:
    def __init__(self, a):
        self.a = a
        self.out = os.path.abspath(a.out)
        os.makedirs(self.out, exist_ok=True)
        self.ph = open(os.path.join(self.out, "phases.jsonl"), "a", buffering=1)
        self.procs = []
        self.stop = threading.Event()

    def phase(self, name, **kv):
        kv.update(phase=name, t=now(), utc=utc())
        self.ph.write(json.dumps(kv) + "\n")
        print(f"[{utc()}] {name} {json.dumps({k: v for k, v in kv.items() if k not in ('phase', 't', 'utc')})}", flush=True)

    def ssh(self, ip, cmd, timeout=60, check=True):
        r = subprocess.run(SSH + [f"ubuntu@{ip}", cmd], capture_output=True, text=True, timeout=timeout)
        if check and r.returncode != 0:
            raise RuntimeError(f"ssh {ip} {cmd[:80]}: rc={r.returncode} {r.stderr[-300:]}")
        return r.stdout

    def bg(self, args, log, env=None):
        p = subprocess.Popen(args, stdout=open(log, "a"), stderr=subprocess.STDOUT, env=env, start_new_session=True)
        self.procs.append(p)
        return p


def my_region():
    tok = urllib.request.urlopen(urllib.request.Request("http://169.254.169.254/latest/api/token", method="PUT",
                                 headers={"X-aws-ec2-metadata-token-ttl-seconds": "60"}), timeout=2).read().decode()
    return urllib.request.urlopen(urllib.request.Request("http://169.254.169.254/latest/meta-data/placement/region",
                                  headers={"X-aws-ec2-metadata-token": tok}), timeout=2).read().decode()


def get(url, token, timeout=10):
    req = urllib.request.Request(url, headers={"Authorization": f"Bearer {token}"})
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())


def post(url, token, body, timeout=60):
    req = urllib.request.Request(url, data=json.dumps(body).encode(), method="POST",
                                 headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(req, timeout=timeout).read())


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def tunnel_loop(run, ip, port, log):
    """One ssh -L per remote-region host, restarted if it drops (until the run stops or the host is gone)."""
    while not run.stop.is_set():
        p = subprocess.Popen(SSH + ["-N", "-o", "ExitOnForwardFailure=yes", "-L", f"127.0.0.1:{port}:127.0.0.1:7070", f"ubuntu@{ip}"],
                             stdout=open(log, "a"), stderr=subprocess.STDOUT)
        while p.poll() is None and not run.stop.is_set():
            time.sleep(1)
        if p.poll() is None:
            p.terminate()
        with open(log, "a") as f:
            f.write(f"{utc()} tunnel to {ip} ended rc={p.returncode}\n")
        time.sleep(2)


HOST_SAMPLER = r'''
import json, os, time, sys
out = open("/var/lib/drums/t3-host-samples.jsonl", "a", buffering=1)
prev = None
while True:
    m = {}
    for l in open("/proc/meminfo"):
        k, v = l.split(":"); m[k] = int(v.split()[0]) * 1024
    c = [int(x) for x in open("/proc/stat").readline().split()[1:]]
    busy = None
    if prev:
        d = [a - b for a, b in zip(c, prev)]; tot = sum(d) or 1
        busy = round(100.0 * (tot - d[3] - d[4]) / tot, 2); iow = round(100.0 * d[4] / tot, 2)
    prev = c
    psi = {}
    for r in ("cpu", "memory", "io"):
        try:
            for l in open(f"/proc/pressure/{r}"):
                p = l.split(); psi[f"{r}_{p[0]}_avg10"] = float(p[1].split("=")[1])
        except OSError:
            pass
    row = {"t": round(time.time(), 2), "mem_total": m["MemTotal"], "mem_available": m["MemAvailable"], "shmem": m.get("Shmem"),
           "slab": m.get("Slab"), "load": open("/proc/loadavg").read().split()[:3], "cpu_busy_pct": busy,
           "cpu_iowait_pct": iow if busy is not None else None, "psi": psi, "nproc_tasks": int(open("/proc/loadavg").read().split()[3].split("/")[1])}
    out.write(json.dumps(row) + "\n")
    time.sleep(5)
'''


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--hosts", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--workers", type=int, default=2100)
    ap.add_argument("--procs", type=int, default=12)
    ap.add_argument("--warmup-s", type=float, default=900)
    ap.add_argument("--ramp-s", type=float, default=600)
    ap.add_argument("--measure-s", type=float, default=3600)
    ap.add_argument("--kill-at-s", type=float, default=1800, help="seconds after the window start; <0 = no kill")
    ap.add_argument("--kill-region", default="")
    ap.add_argument("--max-envs", type=int, default=280)
    ap.add_argument("--max-envs-per-vcpu", type=float, default=0.0, help="if > 0: max_envs = this x the host's vCPUs (mixed fleets)")
    ap.add_argument("--warm-per-host", type=int, default=0)
    ap.add_argument("--max-load-per-cpu", type=float, default=4.0)
    ap.add_argument("--release", default=os.path.expanduser("~/drums-release.tar.gz"))
    ap.add_argument("--seed", type=int, default=20261108)
    ap.add_argument("--steps", type=int, default=15)
    ap.add_argument("--think-min-s", type=float, default=4.0)
    ap.add_argument("--think-max-s", type=float, default=8.0)
    ap.add_argument("--max-inflight-per-host", type=int, default=16)
    a = ap.parse_args()
    run = Run(a)
    hosts = [h for h in json.load(open(a.hosts)) if h.get("role") == "host" and h.get("state") == "running"]
    region = my_region()
    run.phase("start", hosts=len(hosts), coordinator_region=region, args=vars(a))
    json.dump(hosts, open(os.path.join(run.out, "hosts.json"), "w"), indent=1)

    # ---------------------------------------------------------------- 1. hosts
    os.makedirs("/etc/drums-pool/hosts", exist_ok=True)
    specs = []
    by_name = {}
    regions = sorted({h["region"] for h in hosts})
    count = {r: 0 for r in regions}
    for h in sorted(hosts, key=lambda x: (x["region"], x["id"])):
        count[h["region"]] += 1
        h["name"] = f"{h['region'].replace('us-', '').replace('-', '')}-{count[h['region']]:02d}"   # e.g. east2-01
        h["same_region"] = h["region"] == region
        h["ssh_ip"] = h["private_ip"] if h["same_region"] else h["public_ip"]
    deadline = now() + 1800
    pending = list(hosts)
    while pending and now() < deadline:
        for h in list(pending):
            try:
                fb = run.ssh(h["ssh_ip"], "cat /var/lib/drums/firstboot.json", timeout=20, check=True)
                h["firstboot"] = json.loads(fb)
                pending.remove(h)
            except Exception:  # noqa: BLE001
                pass
        if pending:
            time.sleep(10)
    run.phase("hosts_booted", ok=[h["name"] for h in hosts if "firstboot" in h], missing=[h["id"] for h in pending])
    hosts = [h for h in hosts if "firstboot" in h]
    for h in hosts:
        tok = run.ssh(h["ssh_ip"], "sudo cat /etc/drums/token", timeout=20).strip()
        tf = f"/etc/drums-pool/hosts/{h['name']}.token"
        fd = os.open(tf, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        os.write(fd, tok.encode())
        os.close(fd)
        h["token_file"] = tf
        if h["same_region"]:
            h["url"] = f"http://{h['private_ip']}:7070"
        else:
            port = free_port()
            h["url"] = f"http://127.0.0.1:{port}"
            threading.Thread(target=tunnel_loop, args=(run, h["public_ip"], port, os.path.join(run.out, f"tunnel-{h['name']}.log")), daemon=True).start()
        # the host sampler (5 s) starts now and runs until the host stops
        run.ssh(h["ssh_ip"], "sudo systemd-run --unit t3-sampler --collect python3 -c " + shlex.quote(HOST_SAMPLER) + " || true", timeout=30, check=False)
        by_name[h["name"]] = h
        specs.append({"name": h["name"], "url": h["url"], "token_file": tf, "region": h["region"], "cpus": h["firstboot"]["nproc"],
                      "max_envs": int(round(a.max_envs_per_vcpu * h["firstboot"]["nproc"])) if a.max_envs_per_vcpu > 0 else a.max_envs})
    # wait for every drumsd (tunnels included) to answer health with its origin serving
    for h in hosts:
        t = now()
        while now() - t < 900:
            try:
                hv = get(h["url"] + "/v1/health", open(h["token_file"]).read().strip(), timeout=10)
                if hv.get("ok"):
                    h["health0"] = {k: hv.get(k) for k in ("version", "image_sha256", "criu", "limits", "slots_by_state")}
                    break
            except Exception:  # noqa: BLE001
                pass
            time.sleep(5)
    run.phase("hosts_healthy", hosts={h["name"]: (h["region"], h["type"], h.get("health0") is not None) for h in hosts})

    # acceptance: every host must make one real desktop with a task, pass its gates and return the designed no-op reward;
    # a host that cannot is left out of the pool (and named), so a broken origin never takes part in the measurement
    def accept(h):
        tok = open(h["token_file"]).read().strip()
        t0 = time.time()
        try:
            task = get(h["url"] + "/v1/tasks/93eabf48-6a27-4cb6-b963-7d5fe1e0d3a9", tok, timeout=30)
            rec = post(h["url"] + "/v1/slots", tok, {"checkpoint": "osworld-ubuntu", "task": task, "idle_timeout_s": 600}, timeout=300)
            sid = rec.get("id")
            ok = rec.get("state") == "ready"
            verdict = None
            for _ in range(60):
                g = get(h["url"] + f"/v1/slots/{sid}/gates", tok, timeout=30)
                verdict = g["validity"]["verdict"]
                if verdict != "pending":
                    break
                time.sleep(1)
            rw = post(h["url"] + f"/v1/slots/{sid}/verify", tok, {}, timeout=300) if ok and verdict == "valid" else {}
            urllib.request.urlopen(urllib.request.Request(h["url"] + f"/v1/slots/{sid}", method="DELETE", headers={"Authorization": f"Bearer {tok}"}), timeout=60).read()
            h["acceptance"] = {"state": rec.get("state"), "verdict": verdict, "valid": rw.get("valid"), "score": rw.get("score"), "s": round(time.time() - t0, 1)}
            h["accepted"] = bool(ok and verdict == "valid" and rw.get("valid") and rw.get("score") == 0.0)
        except Exception as e:  # noqa: BLE001
            h["acceptance"] = {"error": str(e)[:300], "s": round(time.time() - t0, 1)}
            h["accepted"] = False
    ths = [threading.Thread(target=accept, args=(h,)) for h in hosts]
    [t.start() for t in ths]
    [t.join() for t in ths]
    run.phase("hosts_accepted", accepted=[h["name"] for h in hosts if h["accepted"]],
              rejected={h["name"]: h["acceptance"] for h in hosts if not h["accepted"]}, timings={h["name"]: h["acceptance"].get("s") for h in hosts})
    hosts = [h for h in hosts if h["accepted"]]
    specs = [sp for sp in specs if sp["name"] in {h["name"] for h in hosts}]
    json.dump(hosts, open(os.path.join(run.out, "hosts.json"), "w"), indent=1)

    # ---------------------------------------------------------------- 2. control plane + observability
    tokf = "/etc/drums-pool/token"
    if not os.path.exists(tokf):
        import secrets
        fd = os.open(tokf, os.O_WRONLY | os.O_CREAT, 0o600)
        os.write(fd, secrets.token_urlsafe(32).encode())
        os.close(fd)
    token = open(tokf).read().strip()
    cfg = {"listen": "127.0.0.1:7080", "token_file": tokf, "hosts": specs, "warm_per_host": a.warm_per_host, "max_load_per_cpu": a.max_load_per_cpu, "max_inflight_per_host": a.max_inflight_per_host,
           "event_log": os.path.join(run.out, "pool-events.jsonl"), "default_idle_timeout_s": 900}
    json.dump(cfg, open("/etc/drums-pool/pool.json", "w"), indent=1)
    json.dump(cfg, open(os.path.join(run.out, "pool-config.json"), "w"), indent=1)
    run.bg([f"{O}/bin/drums-pool", "--config", "/etc/drums-pool/pool.json"], os.path.join(run.out, "pool.log"))
    for _ in range(100):
        try:
            urllib.request.urlopen("http://127.0.0.1:7080/healthz", timeout=1)
            break
        except Exception:  # noqa: BLE001
            time.sleep(0.1)
    d = os.path.join(REPO, "pool", "deploy")
    prom_dir = "/var/lib/drums-pool/prom"
    os.makedirs(prom_dir, exist_ok=True)
    open("/etc/drums-pool/prometheus.yml", "w").write(open(os.path.join(d, "prometheus.yml.tmpl")).read().replace("__DIR__", d).replace("__TOKEN__", tokf))
    run.bg([f"{O}/prometheus/prometheus", "--config.file=/etc/drums-pool/prometheus.yml", f"--storage.tsdb.path={prom_dir}", "--web.listen-address=127.0.0.1:9090",
            "--web.enable-admin-api"], os.path.join(run.out, "prometheus.log"))
    run.bg([f"{O}/alertmanager/alertmanager", f"--config.file={d}/alertmanager.yml", "--storage.path=/var/lib/drums-pool/am", "--web.listen-address=127.0.0.1:9093",
            "--cluster.listen-address="], os.path.join(run.out, "alertmanager.log"))
    run.bg([sys.executable, os.path.join(d, "alert_log.py"), os.path.join(run.out, "alerts.jsonl")], os.path.join(run.out, "alert_log.log"))
    gp = "/var/lib/drums-pool/grafana"
    os.makedirs(f"{gp}/prov/datasources", exist_ok=True)
    os.makedirs(f"{gp}/prov/dashboards", exist_ok=True)
    open(f"{gp}/prov/datasources/prom.yml", "w").write("apiVersion: 1\ndatasources:\n- name: prom\n  uid: prom\n  type: prometheus\n  url: http://127.0.0.1:9090\n  isDefault: true\n")
    open(f"{gp}/prov/dashboards/d.yml", "w").write(f"apiVersion: 1\nproviders:\n- name: drums\n  type: file\n  options: {{path: {d}/dashboards}}\n")
    genv = dict(os.environ, GF_PATHS_PROVISIONING=f"{gp}/prov", GF_PATHS_DATA=f"{gp}/data", GF_SERVER_HTTP_ADDR="127.0.0.1", GF_SERVER_HTTP_PORT="3000",
                GF_AUTH_ANONYMOUS_ENABLED="true", GF_AUTH_ANONYMOUS_ORG_ROLE="Viewer", GF_ANALYTICS_REPORTING_ENABLED="false", GF_ANALYTICS_CHECK_FOR_UPDATES="false")
    run.bg([f"{O}/grafana/bin/grafana", "server", f"--homepath={O}/grafana"], os.path.join(run.out, "grafana.log"), env=genv)
    t = now()
    while now() - t < 300:
        hs = get("http://127.0.0.1:7080/pool/v1/hosts", token)["hosts"]
        if all(x["state"] == "healthy" for x in hs):
            break
        time.sleep(2)
    run.phase("pool_up", hosts={x["name"]: x["state"] for x in hs})

    # snapshots + latency probes, every 10 s / 5 s, until the run stops
    def snapper():
        f = open(os.path.join(run.out, "pool-snapshots.jsonl"), "a", buffering=1)
        while not run.stop.is_set():
            try:
                f.write(json.dumps({"t": now(), "hosts": get("http://127.0.0.1:7080/pool/v1/hosts", token)["hosts"],
                                    "health": get("http://127.0.0.1:7080/v1/health", token)}) + "\n")
            except Exception as e:  # noqa: BLE001
                f.write(json.dumps({"t": now(), "error": str(e)}) + "\n")
            run.stop.wait(10)

    def prober():
        # HTTP round trip coordinator -> each drumsd (/healthz, no auth, no work) and -> the pool; the inter-region cost
        f = open(os.path.join(run.out, "rtt.jsonl"), "a", buffering=1)
        while not run.stop.is_set():
            row = {"t": now()}
            for h in hosts:
                t0 = time.perf_counter()
                try:
                    urllib.request.urlopen(h["url"] + "/healthz", timeout=3).read()
                    row[h["name"]] = round(time.perf_counter() - t0, 5)
                except Exception:  # noqa: BLE001
                    row[h["name"]] = None
            t0 = time.perf_counter()
            try:
                urllib.request.urlopen("http://127.0.0.1:7080/healthz", timeout=3).read()
                row["pool"] = round(time.perf_counter() - t0, 5)
            except Exception:  # noqa: BLE001
                row["pool"] = None
            f.write(json.dumps(row) + "\n")
            run.stop.wait(5)
    threading.Thread(target=snapper, daemon=True).start()
    threading.Thread(target=prober, daemon=True).start()

    # ---------------------------------------------------------------- 3. trainer
    tdir = os.path.join(run.out, "trainer")
    total = a.warmup_s + a.measure_s
    t_start = now()
    window = (t_start + a.warmup_s, t_start + total)
    run.phase("trainer_start", workers=a.workers, window_start_utc=utc(window[0]), window_end_utc=utc(window[1]), window=window)
    tr = run.bg([sys.executable, os.path.join(REPO, "pool", "loadgen", "replay_trainer.py"), "--url", "http://127.0.0.1:7080", "--token-file", tokf,
                 "--workers", str(a.workers), "--procs", str(a.procs), "--duration-s", str(total), "--ramp-s", str(a.ramp_s), "--seed", str(a.seed),
                 "--steps", str(a.steps), "--think-min-s", str(a.think_min_s), "--think-max-s", str(a.think_max_s),
                 "--out", tdir], os.path.join(run.out, "trainer.log"))

    # ---------------------------------------------------------------- 4. kill
    killed = None
    if a.kill_at_s >= 0:
        t_kill = window[0] + a.kill_at_s
        while now() < t_kill and tr.poll() is None:
            time.sleep(1)
    if a.kill_at_s >= 0 and tr.poll() is not None:
        run.phase("kill_skipped", why="trainer ended before the kill time", rc=tr.returncode)
    elif a.kill_at_s >= 0:
        hs = get("http://127.0.0.1:7080/pool/v1/hosts", token)["hosts"]
        cands = [x for x in hs if x["state"] == "healthy" and (not a.kill_region or by_name[x["name"]]["region"] == a.kill_region)]
        victim = max(cands, key=lambda x: (x["owned"], x["name"]))
        vh = by_name[victim["name"]]
        run.phase("kill", host=victim["name"], instance=vh["id"], region=vh["region"], owned=victim["owned"], warm=victim["warm"])
        # power off at once: no shutdown, no connection closes (InstanceInitiatedShutdownBehavior=terminate ends the instance)
        try:
            run.ssh(vh["ssh_ip"], "sudo sh -c 'echo 1 > /proc/sys/kernel/sysrq; echo o > /proc/sysrq-trigger' & sleep 1; exit 0", timeout=15, check=False)
        except Exception:  # noqa: BLE001
            pass
        killed = victim["name"]
        run.phase("kill_sent", host=killed)

    tr.wait()
    run.phase("trainer_end", rc=tr.returncode)

    # ---------------------------------------------------------------- 5. collect
    time.sleep(15)
    pdir = os.path.join(run.out, "prom")
    os.makedirs(pdir, exist_ok=True)
    queries = {
        "envs_by_status": "sum by (status) (pool_envs)", "host_envs": "pool_host_envs", "host_up": "pool_host_up",
        "host_mem_avail": "pool_host_mem_available_bytes", "host_mem_total": "pool_host_mem_total_bytes", "host_load1": "pool_host_load1",
        "host_saturation": "pool_host_saturation", "env_mib_est": "pool_host_env_mem_mib_estimate", "queue_depth": "pool_queue_depth",
        "health_rtt": "pool_host_health_rtt_seconds", "verify_rate": "sum by (valid, checker_error) (rate(pool_verify_total[1m]))",
        "requests_rate": "sum by (op, code) (rate(pool_requests_total[1m]))",
        "blocked_rate": "sum by (reason) (rate(pool_place_blocked_total[1m]))",
    }
    for op in ("step", "reset", "create", "verify", "close", "get", "screenshot", "branch"):
        for qn in (0.5, 0.95, 0.99):
            queries[f"req_{op}_p{int(qn * 100)}"] = f'histogram_quantile({qn}, sum by (le) (rate(pool_request_seconds_bucket{{op="{op}"}}[1m])))'
            queries[f"ovh_{op}_p{int(qn * 100)}"] = f'histogram_quantile({qn}, sum by (le) (rate(pool_overhead_seconds_bucket{{op="{op}"}}[1m])))'
    queries["up_step_p50_by_host"] = 'histogram_quantile(0.5, sum by (le, host) (rate(pool_upstream_seconds_bucket{op="step"}[1m])))'
    for name, qq in queries.items():
        u = "http://127.0.0.1:9090/api/v1/query_range?" + urllib.parse.urlencode({"query": qq, "start": t_start - 60, "end": now(), "step": "10"})
        try:
            open(os.path.join(pdir, f"{name}.json"), "wb").write(urllib.request.urlopen(u, timeout=60).read())
        except Exception as e:  # noqa: BLE001
            open(os.path.join(pdir, f"{name}.error"), "w").write(str(e))
    try:
        open(os.path.join(run.out, "pool-metrics-final.prom"), "w").write(
            urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:7080/metrics", headers={"Authorization": f"Bearer {token}"}), timeout=30).read().decode())
        snap = json.loads(urllib.request.urlopen(urllib.request.Request("http://127.0.0.1:9090/api/v1/admin/tsdb/snapshot", method="POST"), timeout=120).read())
        sn = snap["data"]["name"]
        subprocess.run(["tar", "-C", f"{prom_dir}/snapshots", "-czf", os.path.join(run.out, "prom-tsdb.tgz"), sn], check=False)
    except Exception as e:  # noqa: BLE001
        run.phase("collect_warning", what="metrics/tsdb", error=str(e))
    for h in hosts:
        if h["name"] == killed:
            continue
        hd = os.path.join(run.out, "host", h["name"])
        os.makedirs(hd, exist_ok=True)
        try:
            tok = open(h["token_file"]).read().strip()
            json.dump(get(h["url"] + "/v1/metrics", tok, timeout=60), open(os.path.join(hd, "drumsd-metrics.json"), "w"))
            json.dump(get(h["url"] + "/v1/health", tok, timeout=60), open(os.path.join(hd, "drumsd-health.json"), "w"))
        except Exception as e:  # noqa: BLE001
            open(os.path.join(hd, "drumsd.error"), "w").write(str(e))
        subprocess.run(["scp"] + SSH[1:] + [f"ubuntu@{h['ssh_ip']}:/var/lib/drums/t3-host-samples.jsonl", f"ubuntu@{h['ssh_ip']}:/var/lib/drums/firstboot.json", hd],
                       capture_output=True, timeout=300)
        r = run.ssh(h["ssh_ip"], "sudo journalctl -u drumsd --since '-3h' --no-pager -o short-iso | gzip -c | base64 -w0", timeout=300, check=False)
        try:
            import base64
            open(os.path.join(hd, "drumsd-journal.txt.gz"), "wb").write(base64.b64decode(r))
        except Exception:  # noqa: BLE001
            pass
    run.stop.set()
    run.phase("collected")
    for p in run.procs:
        if p is not tr and p.poll() is None:
            p.terminate()
    run.phase("done", out=run.out)
    run.ph.close()
    subprocess.run(f"cd {shlex.quote(run.out)} && find . -type f ! -name SHA256SUMS -print0 | sort -z | xargs -0 sha256sum > SHA256SUMS", shell=True)


if __name__ == "__main__":
    main()
