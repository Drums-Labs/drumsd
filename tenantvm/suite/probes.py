"""Two-tenant escape / side-channel probes. Each probe(a, b) -> dict with keys: name, dim, verdict (PASS|FAIL),
detail, evidence. 'a' and 'b' are Probe handles that run a shell command inside tenant A's / tenant B's desktop env
(as the unprivileged desktop user, via the Drums SDK env.step execute action) and return (rc, stdout, stderr).

A probe is PASS when A is isolated from B / the host / metadata. FAIL means A observed or affected something it must not.
Probes never need host root; they are what a model-controlled agent could do inside a desktop."""
import json, re, time

MARK = "t4probe-7f3a"   # B writes this; A must not find it

def _mark_in(text):   # True if B's secret marker leaked into A's view
    return MARK in (text or "")

def p_fs_home(a, b):
    # B writes a secret file on its Desktop; A tries to read every home it can see.
    b.sh(f"echo {MARK}-$(hostname) > ~/Desktop/SECRET_B.txt; sync")
    rc, out, err = a.sh("cat /home/*/Desktop/SECRET_B.txt /home/*/SECRET_B.txt 2>/dev/null; "
                        "grep -rl t4probe-7f3a /home /root /tmp /var/tmp 2>/dev/null | head")
    leak = _mark_in(out)
    return dict(name="fs.home_read", dim="filesystem", verdict="FAIL" if leak else "PASS",
                detail="A read B's Desktop secret" if leak else "A cannot see B's files",
                evidence=(out or err)[:400])

def p_fs_hostroot(a, b):
    # A tries to read host-owned paths that would exist only if its mount namespace leaked the host root.
    rc, out, err = a.sh("cat /etc/drums/token 2>/dev/null; ls -la /var/lib/drums/state 2>/dev/null | head; "
                        "cat /proc/1/root/etc/shadow 2>/dev/null | head -1")
    leak = bool(out.strip())
    return dict(name="fs.host_root", dim="filesystem", verdict="FAIL" if leak else "PASS",
                detail="A read host files" if leak else "host root not reachable",
                evidence=(out or err)[:400])

def p_net_peer(a, b):
    # Can A reach B over IP? In vm mode B is 10.200.2.2; in bare mode both have loopback only, so there is no peer IP.
    bip = b.peer_ip()
    if not bip:
        rc, out, err = a.sh("ip -br a; ip route")
        return dict(name="net.peer", dim="network", verdict="PASS",
                    detail="desktop has loopback only (no route to any peer)", evidence=(out or "")[:300])
    rc, out, err = a.sh(f"timeout 4 bash -c 'echo > /dev/tcp/{bip}/22' 2>&1 && echo OPEN || echo BLOCKED; "
                        f"ping -c1 -W2 {bip} >/dev/null 2>&1 && echo PING_OK || echo PING_BLOCKED")
    leak = "OPEN" in out or "PING_OK" in out
    return dict(name="net.peer", dim="network", verdict="FAIL" if leak else "PASS",
                detail=f"A reached B at {bip}" if leak else f"A cannot reach B ({bip})",
                evidence=(out or err)[:300])

def p_net_metadata(a, b):
    rc, out, err = a.sh("timeout 4 curl -s -o /dev/null -w '%{http_code}' "
                        "http://169.254.169.254/latest/meta-data/iam/security-credentials/ 2>&1; echo")
    code = (out or "").strip().splitlines()[-1] if out.strip() else ""
    leak = code.startswith("2")
    return dict(name="net.metadata", dim="metadata", verdict="FAIL" if leak else "PASS",
                detail=f"IMDS returned {code}" if leak else "IMDS unreachable",
                evidence=(out or err)[:200])

def p_net_egress_policy(a, b):
    # With the default per-desktop egress policy the desktop has no route out at all; record what it can reach.
    rc, out, err = a.sh("timeout 5 curl -s -o /dev/null -w '%{http_code}' https://1.1.1.1 2>&1; echo /; "
                        "timeout 5 getent hosts example.com 2>&1 | head -1")
    return dict(name="net.egress", dim="network", verdict="INFO",
                detail="desktop outbound reachability (expected: none by default)", evidence=(out or err)[:200])

def p_proc_table(a, b):
    # B starts a uniquely named process (comm ZZSLEEPERMARK); A looks for it by EXACT comm with pgrep -x, whose own comm
    # is "pgrep", so it never matches itself (the earlier grep-on-args test matched its own command line).
    b.sh("setsid bash -c 'exec -a ZZSLEEPERMARK sleep 600' >/dev/null 2>&1 &")
    time.sleep(1.0)
    rc, out, err = a.sh("pgrep -x ZZSLEEPERMARK | wc -l")
    seen = int((out or "0").strip() or 0)
    leak = seen > 0
    return dict(name="proc.table", dim="process", verdict="FAIL" if leak else "PASS",
                detail=f"A saw {seen} of B's named processes" if leak else "A cannot see B's processes",
                evidence=(out or err)[:200])

def p_proc_pidns(a, b):
    # Whether A and B share a pid namespace. pid-ns inodes are only comparable within one kernel, so first check if A and
    # B run on the same kernel (same boot_id). Different boot_id => different kernels (the VM arm) => separate pid
    # namespaces by construction. Same boot_id (bare) => compare the pid-ns inode each sees for /proc/self.
    _, ba, _ = a.sh("cat /proc/sys/kernel/random/boot_id")
    _, bb, _ = b.sh("cat /proc/sys/kernel/random/boot_id")
    ba, bb = ba.strip(), bb.strip()
    if ba and bb and ba != bb:
        return dict(name="proc.pidns", dim="process", verdict="PASS",
                    detail="separate kernels (different boot_id) => separate pid namespaces", evidence=f"bootA={ba[:8]} bootB={bb[:8]}")
    _, oa, _ = a.sh("readlink /proc/self/ns/pid")
    _, ob, _ = b.sh("readlink /proc/self/ns/pid")
    oa, ob = oa.strip(), ob.strip()
    leak = bool(oa) and oa == ob
    return dict(name="proc.pidns", dim="process", verdict="FAIL" if leak else "PASS",
                detail=f"A and B share pid ns {oa}" if leak else f"same kernel, separate pid namespaces (A {oa}, B {ob})",
                evidence=f"A={oa} B={ob}")

def p_proc_global(a, b):
    rc, out, err = a.sh("ps -eo pid,comm 2>/dev/null | awk '$1<50' | wc -l; cat /proc/1/comm 2>/dev/null")
    return dict(name="proc.pid1", dim="process", verdict="INFO",
                detail="what /proc/1 is from A's view", evidence=(out or err)[:150])

def p_proc_sysinfo(a, b):
    rc, out, err = a.sh("cat /proc/loadavg; head -3 /proc/meminfo; nproc")
    return dict(name="proc.sysinfo", dim="/proc", verdict="INFO",
                detail="host-wide vs per-tenant resource view", evidence=(out or err)[:300])

def p_cgroup(a, b):
    rc, out, err = a.sh("cat /proc/self/cgroup; cat /sys/fs/cgroup/cgroup.procs 2>/dev/null | wc -l; "
                        "ls /sys/fs/cgroup 2>/dev/null | head")
    # FAIL if A can enumerate a host-wide cgroup root (many procs, sibling cgroup names visible)
    rc2, out2, _ = a.sh("ls /sys/fs/cgroup 2>/dev/null | grep -Ec 'slot|env|machine|system.slice'")
    sib = int((out2 or "0").strip() or 0)
    return dict(name="cgroup.view", dim="cgroup", verdict="INFO",
                detail=f"sibling-looking cgroup entries visible: {sib}", evidence=(out or err)[:300])

def p_device(a, b):
    rc, out, err = a.sh("ls -la /dev | wc -l; ls /dev/kvm /dev/mem /dev/sda /dev/nvme0n1 /dev/vda 2>&1 | head; "
                        "cat /dev/mem 2>&1 | head -c0; echo mem_rc=$?")
    bad = re.findall(r"/dev/(kvm|mem|nvme0n1|sda)\b", out or "")
    present = [x for x in bad if "No such" not in out]
    leak = any(f"/dev/{d}\n" in (out+"\n") and "cannot" not in out and "No such" not in out for d in ("kvm","mem","nvme0n1","sda"))
    # robust: FAIL only if a host block/mem device node is actually present
    rc2, out2, _ = a.sh("test -e /dev/mem && echo MEM; test -e /dev/kvm && echo KVM; test -e /dev/nvme0n1 && echo NVME; test -e /dev/sda && echo SDA; true")
    leaks = [l for l in (out2 or '').split() if l in ("MEM","KVM","NVME","SDA")]
    return dict(name="device.nodes", dim="device", verdict="FAIL" if leaks else "PASS",
                detail=f"host device nodes present: {leaks}" if leaks else "only the safe desktop device set",
                evidence=(out or err)[:300])

def p_timing_covert(a, b):
    # A crude shared-CPU covert channel: B either spins or idles for 3 windows; A times a fixed workload each window.
    import statistics
    def a_sample():
        rc, out, _ = a.sh("python3 - <<'E'\nimport time\nt=time.perf_counter()\nx=0\nfor i in range(8_000_000):\n x+=i*i\nprint(round((time.perf_counter()-t)*1000,1))\nE")
        try: return float((out or "0").strip().splitlines()[-1])
        except Exception: return None
    rows=[]
    for state in ("idle","spin","idle","spin"):
        if state=="spin":
            b.sh("for c in $(seq 1 $(nproc)); do timeout 6 bash -c 'while :; do :; done' & done")
        time.sleep(0.5)
        rows.append((state, a_sample()))
        time.sleep(0.2)
    idle=[v for s,v in rows if s=="idle" and v]; spin=[v for s,v in rows if s=="spin" and v]
    ratio = (statistics.mean(spin)/statistics.mean(idle)) if idle and spin and statistics.mean(idle) else None
    # A high ratio = A's compute slows a lot when B spins = a shared-core timing channel. VM pinning should keep it ~1.
    verdict = "INFO"
    return dict(name="timing.cpu_covert", dim="timing", verdict=verdict,
                detail=f"A-latency spin/idle ratio={ratio}", evidence=str(rows))

ALL = [p_fs_home, p_fs_hostroot, p_net_peer, p_net_metadata, p_net_egress_policy,
       p_proc_table, p_proc_pidns, p_proc_global, p_proc_sysinfo, p_cgroup, p_device, p_timing_covert]
