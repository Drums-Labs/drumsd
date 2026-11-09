#!/bin/bash
# Run after every (re)boot of a T5 host: remount study filesystems and start drumsd serve-os. Usage: after-boot.sh android|windows
set -u
case $1 in
  android)
    modprobe binder_linux devices="binder,hwbinder,vndbinder" 2>/dev/null
    modprobe xfs; mkdir -p /srv/t5/xfs; mountpoint -q /srv/t5/xfs || mount -o loop,noatime /srv/t5/xfs.img /srv/t5/xfs
    [ -f /root/.emulator_console_auth_token ] || : > /root/.emulator_console_auth_token ;;
  windows)
    D=$(lsblk -dno NAME,SIZE | awk '$2=="1.5T"{print $1}')
    mkdir -p /srv/t5/waa; [ -n "$D" ] && { mountpoint -q /srv/t5/waa || mount -o noatime /dev/$D /srv/t5/waa; } ;;
esac
pgrep -x drumsd >/dev/null || RUST_LOG=info nohup /usr/local/bin/drumsd serve-os --config /etc/drumsd-os.json >> /var/log/drumsd-os.log 2>&1 < /dev/null &
sleep 2; curl -s 127.0.0.1:7070/healthz; echo
