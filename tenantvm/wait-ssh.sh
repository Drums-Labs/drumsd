#!/usr/bin/env bash
# wait-ssh.sh INDEX [TIMEOUT_S]: wait until the guest at 10.200.INDEX.2 answers ssh; prints the seconds it took.
I=${1:?index}; T=${2:-300}; t0=$(date +%s.%N)
while :; do
  if ssh -q -i /root/.ssh/t4_ed25519 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o ConnectTimeout=2 -o BatchMode=yes ubuntu@10.200.$I.2 true 2>/dev/null; then
    echo "$(echo "$(date +%s.%N) - $t0" | bc)"; exit 0; fi
  [ "$(echo "$(date +%s.%N) - $t0 > $T" | bc)" = 1 ] && { echo timeout >&2; exit 1; }
  sleep 0.2
done
