#!/bin/bash
# Auto-stop guard for every T3 host (installed from user-data at launch, before anything else runs).
#   hard ceiling: the host powers off CEILING_MIN minutes after boot, whatever it is doing.
#   idle rule:    the host powers off after IDLE_MIN minutes with no ssh session, no drumsd slot alive and no pool
#                 process running (checked every minute).
# Spot one-time instances are launched with InstanceInitiatedShutdownBehavior=terminate, so power-off ends billing.
set -u
CEILING_MIN="${1:-240}"
IDLE_MIN="${2:-30}"
install -d /etc/drums-guard
echo "ceiling_min=$CEILING_MIN idle_min=$IDLE_MIN booted=$(date -u +%FT%TZ)" > /etc/drums-guard/config
shutdown -P "+$CEILING_MIN" "drums-guard hard ceiling" || true
cat > /usr/local/sbin/drums-guard-idle <<'IDLE'
#!/bin/bash
S=/run/drums-guard-idle-since
IDLE_MIN=$(sed -n 's/.*idle_min=\([0-9]*\).*/\1/p' /etc/drums-guard/config 2>/dev/null)
# no (or a broken) config: never power off on the idle rule (the hard ceiling still holds)
[ -n "$IDLE_MIN" ] && [ "$IDLE_MIN" -gt 0 ] 2>/dev/null || exit 0
busy=0
[ "$(who | wc -l)" -gt 0 ] && busy=1
pgrep -x sshd-session >/dev/null 2>&1 && pgrep -fa 'sshd-session: [a-z]' >/dev/null 2>&1 && busy=1
pgrep -f 'drums-pool|replay_trainer' >/dev/null 2>&1 && busy=1
if [ -s /etc/drums/token ]; then
  n=$(curl -s -m 5 -H "Authorization: Bearer $(cat /etc/drums/token)" http://127.0.0.1:7070/v1/health 2>/dev/null \
      | python3 -c 'import json,sys; d=json.load(sys.stdin); print(sum(v for k,v in d.get("slots_by_state",{}).items() if k!="destroyed"))' 2>/dev/null || echo 0)
  [ "${n:-0}" -gt 0 ] && busy=1
fi
[ -e /etc/drums-guard/hold ] && busy=1
now=$(date +%s)
if [ $busy = 1 ]; then echo "$now" > $S; exit 0; fi
[ -s $S ] || echo "$now" > $S
if [ $(( now - $(cat $S) )) -ge $(( IDLE_MIN * 60 )) ]; then logger "drums-guard: idle ${IDLE_MIN} min, powering off"; systemctl poweroff; fi
IDLE
chmod 755 /usr/local/sbin/drums-guard-idle
cat > /etc/systemd/system/drums-guard-idle.service <<'U'
[Unit]
Description=drums guard idle check
[Service]
Type=oneshot
ExecStart=/usr/local/sbin/drums-guard-idle
U
cat > /etc/systemd/system/drums-guard-idle.timer <<'U'
[Unit]
Description=drums guard idle check every minute
[Timer]
OnBootSec=5min
OnUnitActiveSec=60s
[Install]
WantedBy=timers.target
U
systemctl daemon-reload
systemctl enable --now drums-guard-idle.timer
