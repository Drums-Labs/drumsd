#!/bin/bash
# Make the Drums Windows clean image from WindowsAgentArena's golden storage (after `run-local.sh --prepare-image true`):
#   /srv/t5/win/golden.raw   sparse copy of WAA's data.img (never written again)
#   /srv/t5/win/windows.rom  WAA's UEFI code; clean.vars = its variable store at the saved moment
#   /srv/t5/win/clean.qcow2  overlay on golden.raw holding the disk at the saved moment (frozen, read-only)
#   /srv/t5/win/clean.state  QEMU migration stream of the running VM at that moment (guest server answering)
# QEMU args are read from the drumsd-os config so the state loads with exactly the args it was saved with.
set -euxo pipefail
CFG=${CFG:-/etc/drumsd-os.json}
WAA=${WAA:-/opt/t5/WindowsAgentArena/src/win-arena-container/vm/storage}
SETTLE_S=${SETTLE_S:-180}
W=/srv/t5/win; mkdir -p $W; cd $W
[ -s golden.raw ] || cp --sparse=always $WAA/data.img golden.raw
cp $WAA/windows.rom windows.rom; cp $WAA/windows.vars golden.vars
chmod 444 golden.raw
rm -f clean.qcow2 clean.state clean.vars boot.vars qmp.sock
qemu-img create -q -f qcow2 -F raw -b $W/golden.raw clean.qcow2
cp golden.vars boot.vars
mapfile -t ARGS < <(jq -r '.pools[] | select(.windows) | .windows.vm_args[]' $CFG | sed -e "s#{DISK}#$W/clean.qcow2#" -e "s#{VARS}#$W/boot.vars#")
NIC=$(jq -r '.pools[] | select(.windows) | .windows.nic' $CFG)
UNET=$(jq -r '.pools[] | select(.windows) | .windows.user_net' $CFG)
qemu-system-x86_64 "${ARGS[@]}" -name clean-state -netdev "user,id=n0,hostfwd=tcp:127.0.0.1:7999-:5000,$UNET" -device "$NIC,netdev=n0" \
  -qmp unix:$W/qmp.sock,server=on,wait=off -display none > $W/clean-boot.log 2>&1 &
QPID=$!
t0=$(date +%s)
until curl -s -m 2 127.0.0.1:7999/probe >/dev/null; do sleep 2; [ $(( $(date +%s) - t0 )) -lt 900 ] || { echo "guest server never answered"; kill $QPID; exit 1; }; done
echo "guest server up after $(( $(date +%s) - t0 )) s; settling ${SETTLE_S}s"
sleep $SETTLE_S
curl -s -m 30 -X POST -H 'Content-Type: application/json' -d '{"command": "python -c \"import pyautogui; print(pyautogui.size())\"", "shell": true}' 127.0.0.1:7999/execute | tee $W/clean-pyautogui.json
qmp() { printf '{"execute":"qmp_capabilities"}\n%s\n' "$1" | socat - UNIX-CONNECT:$W/qmp.sock; }
qmp '{"execute":"stop"}'
qmp "{\"execute\":\"migrate\",\"arguments\":{\"uri\":\"exec:cat > $W/clean.state\"}}"
until qmp '{"execute":"query-migrate"}' | grep -q '"status": "completed"'; do sleep 1; done
qmp '{"execute":"query-migrate"}' | tail -1 > $W/clean-migrate.json
cp boot.vars clean.vars
qmp '{"execute":"quit"}' || true
wait $QPID || true
chmod 444 clean.qcow2 clean.state clean.vars
ls -la $W; sha256sum clean.vars windows.rom > $W/SHA256SUMS.small
echo CLEAN_STATE_DONE
