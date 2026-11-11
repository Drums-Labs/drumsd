#!/bin/bash
# delete every live slot on every host in hosts.json (private IP in the coordinator's region, else public IP)
H=${1:-$HOME/hosts-m1.json}; MYR=us-east-2
python3 -c "import json; [print(h['private_ip'] if h['region']=='$MYR' else h['public_ip']) for h in json.load(open('$H')) if h['role']=='host']" | while read ip; do
 ( ssh -i ~/.ssh/t3-coord -o UserKnownHostsFile=~/.ssh/t3_known_hosts -o StrictHostKeyChecking=accept-new -o ConnectTimeout=8 ubuntu@$ip 'T=$(sudo cat /etc/drums/token); for id in $(curl -s -H "Authorization: Bearer $T" localhost:7070/v1/slots | python3 -c "import json,sys; [print(s[\"id\"]) for s in json.load(sys.stdin) if s[\"state\"]!=\"destroyed\"]"); do curl -s -o /dev/null -X DELETE -H "Authorization: Bearer $T" localhost:7070/v1/slots/$id & done; wait; test -f /var/lib/drums/firstboot.json || echo NO-FIRSTBOOT; curl -s -H "Authorization: Bearer $T" localhost:7070/v1/health | python3 -c "import json,sys; d=json.load(sys.stdin); print(d[\"ok\"], {k:v for k,v in d[\"slots_by_state\"].items() if k!=\"destroyed\"})"' 2>&1 | grep -v Warn | sed "s/^/$ip /" ) &
done; wait
