#!/usr/bin/env python3
"""Find what stays in memory after redroid desktops are reset and destroyed (T5 leak). Samples /proc/meminfo fields,
dying cgroups, leftover Android processes and binder state before, during and after a create/reset/destroy cycle.
Usage: leak_probe.py IMAGE N RESETS OUT.jsonl"""
import json, os, subprocess, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from arms import http
img, n, resets, out = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4]
tok = open(os.path.expanduser("~/.drumsd-token")).read().strip(); U = "http://127.0.0.1:7070"
F = open(out, "a")
def sh(c):
    return subprocess.run(c, shell=True, capture_output=True, text=True).stdout
def sample(tag):
    mi = {l.split(":")[0]: int(l.split()[1]) // 1024 for l in open("/proc/meminfo")}
    cg = dict(l.split() for l in open("/sys/fs/cgroup/cgroup.stat"))
    procs = sh("ps -eo comm= | sort | uniq -c | sort -rn | head -12")
    v = {"tag": tag, "t": time.time(), "mem_mib": {k: mi[k] for k in ("MemTotal", "MemAvailable", "Cached", "Shmem", "AnonPages", "Slab", "SUnreclaim", "Unevictable", "Mlocked", "KernelStack", "PageTables", "Percpu", "VmallocUsed")},
         "used_gib": round((mi["MemTotal"] - mi["MemAvailable"]) / 1024, 2), "dying_memcg": int(cg.get("nr_dying_subsys_memory", 0)), "descendants": int(cg.get("nr_descendants", 0)),
         "containers": int(sh("docker ps -aq | wc -l") or 0), "system_server": int(sh("pgrep -c system_server") or 0), "top_procs": procs,
         "binder_stats": sh("sudo cat /sys/kernel/debug/binder/stats 2>/dev/null | head -5"), "binderfs": sh("ls /dev/binderfs 2>/dev/null | wc -l").strip(),
         "drumsd_rss_mib": int(sh("ps -o rss= -C drumsd").split()[0]) // 1024 if sh("ps -o rss= -C drumsd").split() else None}
    F.write(json.dumps(v) + "\n"); F.flush(); print(tag, v["used_gib"], "shmem", v["mem_mib"]["Shmem"], "sunrec", v["mem_mib"]["SUnreclaim"], "anon", v["mem_mib"]["AnonPages"], "dying", v["dying_memcg"], "ss", v["system_server"], "unevict", v["mem_mib"]["Unevictable"], flush=True)
sample("before")
ids = []
for _ in range(n):
    st, v = http("POST", U + "/v1/slots", {"checkpoint": img}, tok); ids.append(v["id"])
sample("created")
for r in range(resets):
    for d in ids:
        http("POST", f"{U}/v1/slots/{d}/reset", {}, tok)
    if (r + 1) % 5 == 0:
        sample(f"reset{r+1}")
for d in ids:
    http("DELETE", f"{U}/v1/slots/{d}", token=tok)
sample("destroyed")
time.sleep(60); sample("destroyed+60s")
subprocess.run("sync; echo 3 | sudo tee /proc/sys/vm/drop_caches >/dev/null", shell=True)
sample("after_drop_caches")
