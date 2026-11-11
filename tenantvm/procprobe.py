import sys, drums, time
url,tok=sys.argv[1],sys.argv[2]
c=drums.connect(url=url,token=tok)
A=c.create(); A.wait_valid(60)
B=c.create(); B.wait_valid(60)
def sh(e,cmd): r=e.step({"type":"execute","command":["bash","-lc",cmd]},check=False).get("result") or {}; return (r.get("stdout") or r.get("output") or "")+((r.get("stderr") or "") )
# ground truth: each env's pid-ns init id and its own pid count
print("A pidns:", sh(A,"readlink /proc/1/ns/pid; echo pid1=$(cat /proc/1/comm); echo npids=$(ls /proc|grep -Ec '^[0-9]+$')").strip())
print("B pidns:", sh(B,"readlink /proc/1/ns/pid; echo pid1=$(cat /proc/1/comm); echo npids=$(ls /proc|grep -Ec '^[0-9]+$')").strip())
# B starts a uniquely-named process
sh(B,"setsid bash -c 'exec -a ZZSLEEPERMARK sleep 400' >/dev/null 2>&1 &"); time.sleep(1)
# A looks by exact comm (pgrep -x won't match its own 'pgrep' comm)
print("A sees ZZSLEEPERMARK:", sh(A,"pgrep -x ZZSLEEPERMARK | wc -l; pgrep -c -x sleep").strip())
print("B sees ZZSLEEPERMARK:", sh(B,"pgrep -x ZZSLEEPERMARK | wc -l").strip())
# signal reach vs own /proc set
print("A killable vs own:", sh(A,"n=0; for p in $(seq 2 5000); do kill -0 $p 2>/dev/null && n=$((n+1)); done; echo killable=$n own=$(ls /proc|grep -Ec '^[0-9]+$')").strip())
A.close(); B.close(); c.close()
