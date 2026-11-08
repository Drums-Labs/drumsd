#!/usr/bin/env python3
"""run_suite.py MODE A_URL A_TOKEN B_URL B_TOKEN OUT.json [TASK]
MODE = bare | vm. Creates one Drums env per tenant (A, B), runs every probe, writes a JSON report.
A_URL/B_URL are drumsd base URLs; in bare mode they are equal. TASK defaults to a no-task blank desktop.
Probes act only through the SDK (env.step execute), i.e. as an in-desktop agent; no host root inside a tenant."""
import json, sys, time
import drums
import probes as P

class Probe:
    def __init__(self, env, peer_ip=None):
        self.env = env; self._peer = peer_ip
    def sh(self, cmd):
        r = self.env.step({"type": "execute", "command": ["bash", "-lc", cmd]}, check=False).get("result") or {}
        return (r.get("returncode"), r.get("stdout") or r.get("output") or "", r.get("stderr") or r.get("error") or "")
    def peer_ip(self):
        return self._peer

def mk(url, token, task):
    c = drums.connect(url=url, token=token)
    e = c.create(task=task) if task else c.create()
    try: e.wait_valid(timeout=90)
    except Exception: pass
    return c, e

def main():
    mode, aurl, atok, burl, btok, out = sys.argv[1:7]
    task = sys.argv[7] if len(sys.argv) > 7 else None
    # In vm mode each tenant's desktop still has only loopback; the peer IP that matters for net.peer is the OTHER
    # tenant VM's address (10.200.2.2 for B) which A would only reach if the host firewall let tap->tap through.
    a_peer_of_b = "10.200.2.2" if mode == "vm" else None
    ca, ea = mk(aurl, atok, task); cb, eb = mk(burl, btok, task)
    a = Probe(ea, peer_ip=a_peer_of_b); b = Probe(eb)
    results = []
    for fn in P.ALL:
        t0 = time.time()
        try:
            r = fn(a, b)
        except Exception as e:
            r = dict(name=fn.__name__, dim="?", verdict="ERROR", detail=str(e), evidence="")
        r["seconds"] = round(time.time() - t0, 2)
        results.append(r)
        print(f"[{mode}] {r['verdict']:5} {r['name']:22} {r['detail']}", flush=True)
    rep = dict(mode=mode, when=time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
               a_url=aurl, b_url=burl, task=task,
               counts={v: sum(1 for r in results if r["verdict"] == v) for v in ("PASS","FAIL","INFO","ERROR")},
               results=results)
    with open(out, "w") as f: json.dump(rep, f, indent=2)
    for c, e in ((ca, ea), (cb, eb)):
        try: e.close()
        except Exception: pass
        try: c.close()
        except Exception: pass
    print(json.dumps(rep["counts"]))
    # exit non-zero if any security probe (PASS/FAIL dimension) failed
    sys.exit(1 if rep["counts"]["FAIL"] or rep["counts"]["ERROR"] else 0)

if __name__ == "__main__":
    main()
