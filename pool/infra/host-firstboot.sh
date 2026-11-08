#!/bin/bash
# First boot of a T3 pool host made from the T3 AMI (run from user-data, after the guard is installed).
#   host-firstboot.sh USD_PER_HOUR
# Sizes the state tmpfs (half of RAM) and the restore permits (vCPUs/4, at least 4) for this instance, then starts the
# Drums units in their reboot order: drums-boot (tmpfs, image mount) -> drumsd -> drums-origin (provisions the clean
# checkpoint). Writes /var/lib/drums/firstboot.json with the timings.
set -euo pipefail
PRICE="${1:-0}"
T0=$(date +%s.%N)
MEM_GIB=$(awk '/MemTotal/{printf "%d", $2/1048576}' /proc/meminfo)
NP=$(nproc)
sed -i "s/^TMPFS_GIB=.*/TMPFS_GIB=$((MEM_GIB / 2))/" /etc/drums/boot.env
python3 - "$NP" "$PRICE" <<'PY'
import json, sys
p = "/etc/drums/drumsd.json"
c = json.load(open(p))
c["max_concurrent_restores"] = max(4, int(sys.argv[1]) // 4)
c["host_usd_per_hour"] = float(sys.argv[2])
json.dump(c, open(p, "w"), indent=1)
PY
systemctl daemon-reload
systemctl enable -q drums-boot drumsd drums-origin
systemctl start drums-boot
T1=$(date +%s.%N)
systemctl start drumsd
systemctl start drums-origin
T2=$(date +%s.%N)
python3 - "$T0" "$T1" "$T2" "$MEM_GIB" "$NP" <<'PY'
import json, sys, socket
t0, t1, t2 = map(float, sys.argv[1:4])
json.dump({"host": socket.gethostname(), "mem_gib": int(sys.argv[4]), "nproc": int(sys.argv[5]), "prepare_s": round(t1 - t0, 1),
           "origin_s": round(t2 - t1, 1), "total_s": round(t2 - t0, 1), "end_epoch": t2}, open("/var/lib/drums/firstboot.json", "w"))
PY
echo "drums firstboot done in $(python3 -c "print(round($T2-$T0,1))") s"
