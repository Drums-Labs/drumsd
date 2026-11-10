#!/bin/bash
# hotadd NAME IP CPUS: wait for first boot, accept (one task desktop, valid, no-op reward 0.0), register with the running pool
NAME=$1; IP=$2; CPUS=$3; LOG=${RAW:-~/raw/run}/hotadd-$NAME.log
S="ssh -i $HOME/.ssh/t3-coord -o UserKnownHostsFile=$HOME/.ssh/t3_known_hosts -o StrictHostKeyChecking=accept-new -o ConnectTimeout=5 -o BatchMode=yes ubuntu@$IP"
echo "$(date -u +%FT%TZ) wait first boot" >> $LOG
until $S "test -f /var/lib/drums/firstboot.json" 2>/dev/null; do sleep 5; done
$S "cat /var/lib/drums/firstboot.json; grep real /var/log/drums-prewarm.log" >> $LOG 2>&1
TF=/etc/drums-pool/hosts/$NAME.token; $S "sudo cat /etc/drums/token" > $TF; chmod 600 $TF
U=http://$IP:7070; HT=$(cat $TF)
python3 - "$U" "$HT" >> $LOG 2>&1 <<'PY'
import json, sys, time, urllib.request
u, t = sys.argv[1], sys.argv[2]
def rq(m, p, b=None):
    r = urllib.request.Request(u + p, method=m, data=None if b is None else json.dumps(b).encode(), headers={"Authorization": f"Bearer {t}", "Content-Type": "application/json"})
    return json.loads(urllib.request.urlopen(r, timeout=300).read() or b"null")
task = rq("GET", "/v1/tasks/93eabf48-6a27-4cb6-b963-7d5fe1e0d3a9")
rec = rq("POST", "/v1/slots", {"checkpoint": "osworld-ubuntu", "task": task, "idle_timeout_s": 600})
for _ in range(60):
    v = rq("GET", f"/v1/slots/{rec['id']}/gates")["validity"]["verdict"]
    if v != "pending": break
    time.sleep(1)
rw = rq("POST", f"/v1/slots/{rec['id']}/verify", {})
rq("DELETE", f"/v1/slots/{rec['id']}")
ok = rec["state"] == "ready" and v == "valid" and rw.get("valid") and rw.get("score") == 0.0
print(json.dumps({"acceptance": {"state": rec["state"], "verdict": v, "valid": rw.get("valid"), "score": rw.get("score")}, "accepted": ok}))
sys.exit(0 if ok else 1)
PY
[ $? = 0 ] || { echo "$(date -u +%FT%TZ) REJECTED" >> $LOG; exit 1; }
PT=$(cat /etc/drums-pool/token)
curl -s -X POST -H "Authorization: Bearer $PT" -H "Content-Type: application/json" localhost:7080/pool/v1/hosts \
  -d "{\"name\": \"$NAME\", \"url\": \"$U\", \"token_file\": \"$TF\", \"region\": \"us-east-2\", \"cpus\": $CPUS, \"max_envs\": $(python3 -c "print(round(4.375*$CPUS))")}" >> $LOG
echo " $(date -u +%FT%TZ) registered" >> $LOG; tail -2 $LOG
