#!/usr/bin/env bash
# Create 2 envs via SDK, then (from the host) show which host uid owns each live slot's upper tree. Demonstrates the
# documented shared host-uid range across envs (the fork defect the uid-range work fixes).
set -e; URL=$1; TOK=$2
~/dv/bin/python - "$URL" "$TOK" <<'PY' &
import sys,drums,time
c=drums.connect(url=sys.argv[1],token=sys.argv[2])
a=c.create(); a.wait_valid(60); b=c.create(); b.wait_valid(60)
open("/tmp/t4envs","w").write(a.id+" "+b.id)
time.sleep=__import__("time").sleep; time.sleep(25)
a.close(); b.close(); c.close()
PY
sleep 20
echo "live slots upper owner (host uid):"
for d in /var/lib/drums/state/slot*/upper; do [ -d "$d" ] && echo "$d -> uid $(sudo stat -c %u "$d")"; done | grep -v " uid 0$" | head
echo "idmapped origin views mounted:"; mount | grep drums-idmap | sed 's/ type.*//'
wait
