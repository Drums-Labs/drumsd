#!/bin/bash
# On a T5 host: destroy live desktops, rebuild and restart drumsd serve-os with the repo config for this host,
# then (optional) start a validation soak per pool: redeploy.sh CONFIG OS POOL [POOL...]
set -u
CFG=$1; OS=$2; shift 2
T=$(cat ~/.drumsd-token)
for s in $(curl -s -H "Authorization: Bearer $T" 127.0.0.1:7070/v1/slots | python3 -c 'import json,sys;print(" ".join(v["id"] for v in json.load(sys.stdin)))' 2>/dev/null); do curl -s -X DELETE -H "Authorization: Bearer $T" 127.0.0.1:7070/v1/slots/$s >/dev/null; done
. ~/.cargo/env; cd ~/drumsd && cargo build --release -p drumsd 2>&1 | tail -1
sudo pkill -x drumsd; sleep 1
sudo install -m 755 target/release/drumsd /usr/local/bin/drumsd
sudo cp "osvm/config/$CFG" /etc/drumsd-os.json
sudo rm -f /var/lib/drumsd-os/journal/clean-probe-* /var/lib/drumsd-os/journal/probe-*
sudo sh -c "RUST_LOG=info nohup /usr/local/bin/drumsd serve-os --config /etc/drumsd-os.json >> /var/log/drumsd-os.log 2>&1 &"
sleep 3
cd ~/drumsd/osvm/harness
for img in "$@"; do
  [ -f ~/t5runs/validate-$img.jsonl ] && mv ~/t5runs/validate-$img.jsonl ~/t5runs/validate-$img-$(date +%H%M%S).jsonl
  nohup python3 soak.py --arm drums --os $OS --image $img --token-file ~/.drumsd-token --n 2 --episodes 48 --tasks ../tasks/$OS --mode mixed --out ~/t5runs/validate-$img.jsonl > ~/t5runs/validate-$img.log 2>&1 < /dev/null &
done
echo deployed
