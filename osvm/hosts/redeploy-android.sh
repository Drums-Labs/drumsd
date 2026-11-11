#!/bin/bash
# on t5-android: destroy live desktops, rebuild + restart drumsd with the repo config, start validation soaks
set -u
T=$(cat ~/.drumsd-token)
for s in $(curl -s -H "Authorization: Bearer $T" 127.0.0.1:7070/v1/slots | python3 -c 'import json,sys;print(" ".join(v["id"] for v in json.load(sys.stdin)))'); do curl -s -X DELETE -H "Authorization: Bearer $T" 127.0.0.1:7070/v1/slots/$s >/dev/null; done
. ~/.cargo/env; cd ~/drumsd && cargo build --release -p drumsd 2>&1 | tail -1
sudo pkill -x drumsd; sleep 1
sudo install -m 755 target/release/drumsd /usr/local/bin/drumsd
sudo cp osvm/config/android-host.json /etc/drumsd-os.json
sudo rm -f /var/lib/drumsd-os/journal/clean-probe-* /var/lib/drumsd-os/journal/probe-*
sudo sh -c "RUST_LOG=info nohup /usr/local/bin/drumsd serve-os --config /etc/drumsd-os.json >> /var/log/drumsd-os.log 2>&1 &"
sleep 3
cd ~/drumsd/osvm/harness
for img in "$@"; do
  [ -f ~/t5runs/validate-$img.jsonl ] && mv ~/t5runs/validate-$img.jsonl ~/t5runs/validate-$img-$(date +%H%M%S).jsonl
  nohup python3 soak.py --arm drums --os android --image $img --token-file ~/.drumsd-token --n 2 --episodes 48 --tasks ../tasks/android --mode mixed --out ~/t5runs/validate-$img.jsonl > ~/t5runs/validate-$img.log 2>&1 < /dev/null &
done
echo deployed
