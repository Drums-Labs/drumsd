#!/bin/bash
# Corrected AndroidWorld latency run (deviation D-A0: the frozen run's driver sent the oracle's wait step to
# /execute_action with a duration, which AndroidWorld rejects; the driver now waits locally). Runs after the
# android clean-path runs.
set -u
until grep -q CLEAN_PATH_DONE ~/t5runs/clean-path-android.log 2>/dev/null; do sleep 60; done
bash ~/drumsd/osvm/harness/run_measured.sh android latency-a0 > /dev/null 2>&1 || true
H=~/drumsd/osvm/harness; OUT=~/t5runs/measured
APKS=/opt/t5/apks/net.gsantner.markor_146.apk,/opt/t5/apks/com.arduia.expense_11.apk,/opt/t5/apks/com.simplemobiletools.calendar.pro_238.apk
python3 $H/soak.py --arm aw --os android --image android_world:55ddaf5 --pool-config $H/../config/android-host.json --pool android-emulator --apks $APKS --configure $H/../config/android-configure.sh --n 16 --episodes 19 --tasks $H/../tasks/android --out $OUT/lat-a0-n16-corrected.jsonl > $OUT/lat-a0-n16-corrected.log 2>&1
echo A0_RERUN_DONE
