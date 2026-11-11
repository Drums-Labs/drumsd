import sys, drums
url, tok = sys.argv[1], sys.argv[2]
c = drums.connect(url=url, token=tok)
h = c.health()
print("health_keys", list(h)[:8])
e = c.create(); e.wait_valid(timeout=90)
r = e.step({"type":"execute","command":["bash","-lc","cat /proc/self/uid_map; grep -E 'NoNewPrivs|Seccomp|CapBnd' /proc/self/status"]}, check=False).get("result") or {}
print("uidmap_sec:", (r.get("stdout") or r.get("output") or r.get("stderr")))
print("valid:", e.valid(timeout=30))
e.close(); c.close()
