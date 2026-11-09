#!/bin/bash
# Windows: after the frozen latency phase, the density ladders (Drums with KSM on, WAA stock with KSM off), then the
# labelled sensitivity cell: WAA with KSM on at its N_max and the next rung.
set -u
until grep -q "PHASE_DONE windows latency" ~/t5runs/run-windows-latency.log 2>/dev/null; do sleep 30; done
H=~/drumsd/osvm/harness
bash $H/run_measured.sh windows ladder w1 4 8 12 16 24 32 40 48
bash $H/run_measured.sh windows ladder w0 4 8 12 16 24 32 40 48
echo CHAIN_WINDOWS_DONE
