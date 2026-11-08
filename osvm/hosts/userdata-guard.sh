#!/bin/bash
# Drums SOTA host guard: hard ceiling + idle rule. Installed at first boot.
# HARD_CEILING_H: power off this many hours after boot, always.
# Idle rule: power off after IDLE_MIN minutes with no established ssh connection (ss, port 22; `who` misses
# non-interactive ssh) AND 1-min load < 1.0,
# unless /run/drums-busy exists (a measurement in progress touches it) and is fresher than 30 min.
HARD_CEILING_H=${HARD_CEILING_H:-16}
IDLE_MIN=${IDLE_MIN:-40}
cat > /usr/local/sbin/drums-guard <<G
#!/bin/bash
idle=0
while true; do
  up=\$(cut -d. -f1 /proc/uptime)
  if [ "\$up" -gt \$(( $HARD_CEILING_H*3600 )) ]; then logger drums-guard hard ceiling; /sbin/poweroff; fi
  ssh_n=\$(ss -Htn state established '( sport = :22 )' | wc -l); load=\$(cut -d' ' -f1 /proc/loadavg)
  busy=0; [ -f /run/drums-busy ] && [ \$(( \$(date +%s) - \$(stat -c %Y /run/drums-busy) )) -lt 1800 ] && busy=1
  if [ "\$ssh_n" -eq 0 ] && [ "\$busy" -eq 0 ] && awk "BEGIN{exit !(\$load < 1.0)}"; then idle=\$((idle+1)); else idle=0; fi
  if [ "\$idle" -ge $IDLE_MIN ]; then logger drums-guard idle; /sbin/poweroff; fi
  echo "\$(date -u +%FT%TZ) up=\$up ssh=\$ssh_n load=\$load busy=\$busy idle=\$idle" > /run/drums-guard.status
  sleep 60
done
G
chmod +x /usr/local/sbin/drums-guard
cat > /etc/systemd/system/drums-guard.service <<S
[Unit]
Description=Drums SOTA auto-stop guard
[Service]
ExecStart=/usr/local/sbin/drums-guard
Restart=always
[Install]
WantedBy=multi-user.target
S
systemctl daemon-reload; systemctl enable --now drums-guard.service
