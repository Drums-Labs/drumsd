#!/usr/bin/env bash
# mktenant.sh NAME INDEX [SRC]: make $T4/NAME.raw from SRC (default base.raw) with the tenant's own identity:
# hostname t4-NAME, static address 10.200.INDEX.2/30 (gateway 10.200.INDEX.1 = the host end of tap-NAME), new machine-id
# and ssh host keys at first boot, and the host's t4 key (root) in ubuntu's authorized_keys.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; . "$HERE/pins.env"
N=${1:?name}; I=${2:?index}; SRC=${3:-$T4/base.raw}
cd "$T4"; [ -f /root/.ssh/t4_ed25519 ] || ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/t4_ed25519
cp --sparse=always "$SRC" "$N.raw.tmp" && mv "$N.raw.tmp" "$N.raw"
M=$(mktemp -d); mount -o loop "$N.raw" "$M"
echo "t4-$N" > "$M/etc/hostname"
cat > "$M/etc/netplan/60-t4.yaml" <<Y
network:
  version: 2
  ethernets:
    eth0:
      addresses: [10.200.$I.2/30]
      routes: [{to: default, via: 10.200.$I.1}]
      nameservers: {addresses: [1.1.1.1, 9.9.9.9]}
Y
chmod 600 "$M/etc/netplan/60-t4.yaml"
: > "$M/etc/machine-id"; rm -f "$M"/etc/ssh/ssh_host_*
install -d -m 700 -o 1000 -g 1000 "$M/home/ubuntu/.ssh"
{ cat /home/ubuntu/.ssh/authorized_keys; cat /root/.ssh/t4_ed25519.pub; } | sort -u > "$M/home/ubuntu/.ssh/authorized_keys"
chown 1000:1000 "$M/home/ubuntu/.ssh/authorized_keys"; chmod 600 "$M/home/ubuntu/.ssh/authorized_keys"
# a tenant's drums API token must be its own (a copied install would otherwise share it)
if [ -f "$M/etc/drums/token" ]; then head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$M/etc/drums/token"; chmod 600 "$M/etc/drums/token"; fi
sync; umount "$M"; rmdir "$M"; echo "$T4/$N.raw ready (10.200.$I.2)"
