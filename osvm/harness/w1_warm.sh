#!/bin/bash
# Labelled follow-up (deviation D-W1W, after the frozen runs): re-save the Windows clean state with WAA's Python 3.10,
# pyautogui and PowerShell warmed inside the guest, then rerun the Drums Windows latency at N=8.
set -u
until grep -q CLEAN_PATH_DONE ~/t5runs/clean-path-windows.log 2>/dev/null; do sleep 60; done
T=$(cat ~/.drumsd-token)
for s in $(curl -s -H "Authorization: Bearer $T" 127.0.0.1:7070/v1/slots | python3 -c 'import json,sys;print(" ".join(v["id"] for v in json.load(sys.stdin)))'); do curl -s -X DELETE -H "Authorization: Bearer $T" 127.0.0.1:7070/v1/slots/$s >/dev/null; done
sudo pkill -x drumsd; sleep 2
sudo mkdir -p /srv/t5/win-cold && sudo cp -a /srv/t5/win/clean.qcow2 /srv/t5/win/clean.state /srv/t5/win/clean.vars /srv/t5/win/clean-*.json /srv/t5/win-cold/ 2>/dev/null
sudo WARM=1 bash ~/drumsd/osvm/hosts/windows-clean-state.sh > ~/t5runs/clean-state-warm.log 2>&1
bash ~/drumsd/osvm/hosts/redeploy.sh windows-host.json windows > /dev/null 2>&1
H=~/drumsd/osvm/harness; OUT=~/t5runs/measured
echo on | true; echo 1 | sudo tee /sys/kernel/mm/ksm/run >/dev/null
python3 $H/soak.py --arm drums --os windows --image windows-11-eval --token-file ~/.drumsd-token --n 8 --episodes 15 --tasks $H/../tasks/windows --out $OUT/lat-w1-n8-warm.jsonl > $OUT/lat-w1-n8-warm.log 2>&1
echo W1_WARM_DONE
