#!/bin/bash
# T5 measured runs (PLAN.md, frozen). Raw files: ~/t5runs/measured/<name>.jsonl (+ .log, .logs/ for incumbents).
#   run_measured.sh android latency
#   run_measured.sh android ladder <arm: a1|a2|a3|a0> <N...>
#   run_measured.sh windows latency
#   run_measured.sh windows ladder <arm: w1|w0> <N...>
# Ladder rule: stop at the first rung that fails the density criteria (analyze.py), after one retry of that rung.
set -u
OS=$1; PHASE=$2; shift 2
OUT=~/t5runs/measured; mkdir -p $OUT
H=$(cd "$(dirname "$0")" && pwd)
APKS=/opt/t5/apks/net.gsantner.markor_146.apk,/opt/t5/apks/com.arduia.expense_11.apk,/opt/t5/apks/com.simplemobiletools.calendar.pro_238.apk
ksm() { # on|off: host KSM for the run (PLAN: Drums Windows arm on, stock WAA off; KSM=on|off overrides)
  if [ "$1" = on ]; then echo 10000 | sudo tee /sys/kernel/mm/ksm/pages_to_scan >/dev/null; echo 10 | sudo tee /sys/kernel/mm/ksm/sleep_millisecs >/dev/null; echo 1 | sudo tee /sys/kernel/mm/ksm/run >/dev/null
  else echo 2 | sudo tee /sys/kernel/mm/ksm/run >/dev/null; echo 0 | sudo tee /sys/kernel/mm/ksm/run >/dev/null; fi
  echo "ksm=$1 run=$(cat /sys/kernel/mm/ksm/run) pages_to_scan=$(cat /sys/kernel/mm/ksm/pages_to_scan) sleep_ms=$(cat /sys/kernel/mm/ksm/sleep_millisecs)"
}
run() { # arm-key n episodes min_seconds name
  local key=$1 n=$2 ep=$3 ms=$4 name=$5
  case $key in w1) ksm ${KSM:-on} > $OUT/$name.ksm ;; w0) ksm ${KSM:-off} > $OUT/$name.ksm ;; esac
  case $key in
    a1) python3 $H/soak.py --arm drums --os android --image android-redroid --token-file ~/.drumsd-token --n $n --episodes $ep --min-seconds $ms --tasks $H/../tasks/android --out $OUT/$name.jsonl ;;
    a2) python3 $H/soak.py --arm drums --os android --image android-emulator --token-file ~/.drumsd-token --n $n --episodes $ep --min-seconds $ms --tasks $H/../tasks/android --out $OUT/$name.jsonl ;;
    a3) python3 $H/soak.py --arm drums --os android --image android-emulator-cow --token-file ~/.drumsd-token --n $n --episodes $ep --min-seconds $ms --tasks $H/../tasks/android --out $OUT/$name.jsonl ;;
    a0) python3 $H/soak.py --arm aw --os android --image android_world:55ddaf5 --pool-config $H/../config/android-host.json --pool android-emulator --apks $APKS --configure $H/../config/android-configure.sh --n $n --episodes $ep --min-seconds $ms --tasks $H/../tasks/android --out $OUT/$name.jsonl ;;
    w1) python3 $H/soak.py --arm drums --os windows --image windows-11-eval --token-file ~/.drumsd-token --n $n --episodes $ep --min-seconds $ms --tasks $H/../tasks/windows --out $OUT/$name.jsonl ;;
    w0) sudo -E python3 $H/soak.py --arm waa --os windows --image "" --pool-config $H/../config/windows-host.json --pool windows-11-eval --n $n --episodes $ep --min-seconds $ms --tasks $H/../tasks/windows --out $OUT/$name.jsonl ;;
  esac > $OUT/$name.log 2>&1
}
passes() { python3 -c "import sys; sys.path.insert(0,'$H'); from analyze import summarize; r=summarize('$OUT/$1.jsonl'); print(r['density_criteria']); sys.exit(0 if r['density_pass'] else 1)"; }
sudo touch /run/drums-busy
if [ "$PHASE" = latency ]; then
  if [ "$OS" = android ]; then for k in a3 a2 a1 a0; do run $k 16 19 0 lat-$k-n16; done; fi
  if [ "$OS" = windows ]; then for k in w1 w0; do run $k 8 15 0 lat-$k-n8; done; fi
elif [ "$PHASE" = ladder ]; then
  key=$1; shift
  for n in "$@"; do
    run $key $n 2 600 lad-$key-n$n
    if passes lad-$key-n$n; then echo "$key N=$n PASS"; continue; fi
    echo "$key N=$n FAIL, retry"; run $key $n 2 600 lad-$key-n$n-retry
    if passes lad-$key-n$n-retry; then echo "$key N=$n PASS (retry)"; continue; fi
    echo "$key N=$n FAIL twice: stop"; break
  done
fi
echo PHASE_DONE $OS $PHASE "$@"
