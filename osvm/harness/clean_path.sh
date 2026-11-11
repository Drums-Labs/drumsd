#!/bin/bash
# The clean reset each incumbent has (PLAN: A0-clean / W0-clean): a new desktop from scratch, 10 sequential runs at
# N=1 (one episode each so the desktop is proven usable). Bring-up time = the "up" record's ready_s.
#   clean_path.sh android|windows   (waits for the host's chain to finish)
set -u
OS=$1; H=~/drumsd/osvm/harness; OUT=~/t5runs/measured
until grep -q "CHAIN_$(echo $OS | tr a-z A-Z)_DONE" ~/t5runs/chain-$OS.log 2>/dev/null; do sleep 60; done
APKS=/opt/t5/apks/net.gsantner.markor_146.apk,/opt/t5/apks/com.arduia.expense_11.apk,/opt/t5/apks/com.simplemobiletools.calendar.pro_238.apk
for i in $(seq 1 10); do
  if [ $OS = android ]; then
    python3 $H/soak.py --arm aw --os android --image android_world:55ddaf5 --pool-config $H/../config/android-host.json --pool android-emulator --apks $APKS --configure $H/../config/android-configure.sh --n 1 --episodes 1 --tasks $H/../tasks/android --out $OUT/clean-a0-$i.jsonl > $OUT/clean-a0-$i.log 2>&1
  else
    sudo -E KSM=off python3 $H/soak.py --arm waa --os windows --image "" --pool-config $H/../config/windows-host.json --pool windows-11-eval --n 1 --episodes 1 --tasks $H/../tasks/windows --out $OUT/clean-w0-$i.jsonl > $OUT/clean-w0-$i.log 2>&1
  fi
done
echo CLEAN_PATH_DONE
