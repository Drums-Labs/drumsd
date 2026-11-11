#!/bin/bash
# stop the T3 run processes on the coordinator (patterns built at run time so this script never matches itself)
p() { pgrep -f "$1" | grep -vx $$ ; }
R=run_scale; T=replay_trainer; for x in $(p "pool/run/$R"); do kill $x; done
for x in $(p "loadgen/$T"); do kill -TERM $x; done
for i in $(seq 1 30); do [ -z "$(p "loadgen/$T")" ] && break; sleep 1; done
for pat in "bin/drums-pool --config" "prometheus/prometheus --config" "alertmanager/alertmanager --config" "deploy/alert_log.py" "grafana/bin/grafana server" "ExitOnForwardFailure=yes"; do
  for x in $(p "$pat"); do kill $x; done; done
sleep 1; ps -eo args | grep -E "drums-pool|$T|$R|prometheus|grafana" | grep -v grep | cut -c1-60; echo stopped
