#!/bin/bash
# Android: after the frozen latency phase, deploy the post-freeze fix (adb transport reopened after snapshot load;
# REPORT deviation D-A3), rerun A3 latency as a labelled run, then the density ladders of every arm.
set -u
until grep -q "PHASE_DONE android latency" ~/t5runs/run-android-latency.log 2>/dev/null; do sleep 30; done
bash ~/drumsd/osvm/hosts/redeploy.sh android-host.json android > ~/t5runs/redeploy-postfix.log 2>&1
H=~/drumsd/osvm/harness; OUT=~/t5runs/measured
python3 $H/soak.py --arm drums --os android --image android-emulator-cow --token-file ~/.drumsd-token --n 16 --episodes 19 --tasks $H/../tasks/android --out $OUT/lat-a3-n16-postfix.jsonl > $OUT/lat-a3-n16-postfix.log 2>&1
bash $H/run_measured.sh android ladder a3 8 16 32 48 64 96 128
bash $H/run_measured.sh android ladder a1 8 16 32 48 64 96 128 160 192 256
bash $H/run_measured.sh android ladder a2 8 16 32 48 64 96 128
bash $H/run_measured.sh android ladder a0 8 16 32 48 64 96 128
echo CHAIN_ANDROID_DONE
