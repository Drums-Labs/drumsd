#!/bin/sh
# Install the Drums slot sysctls (60-drums-slots.conf) and apply them now. Run as root.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
install -m 0644 "$here/60-drums-slots.conf" /etc/sysctl.d/60-drums-slots.conf
sysctl -q -p /etc/sysctl.d/60-drums-slots.conf
for k in fs.inotify.max_user_instances fs.inotify.max_user_watches; do
  want=$(sed -n "s/^$k *= *//p" "$here/60-drums-slots.conf")
  got=$(sysctl -n "$k")
  [ "$got" = "$want" ] || { echo "install_sysctl: $k is $got, want $want" >&2; exit 1; }
done
echo "install_sysctl: fs.inotify.max_user_instances=$(sysctl -n fs.inotify.max_user_instances) fs.inotify.max_user_watches=$(sysctl -n fs.inotify.max_user_watches)"
