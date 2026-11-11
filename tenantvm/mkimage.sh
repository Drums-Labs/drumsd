#!/usr/bin/env bash
# mkimage.sh: build $T4/base.raw, the tenant VM's root disk, as a copy of THIS host's root filesystem.
# Run it on a fresh host booted from HOST_AMI, BEFORE anything else is installed, so the guest and the bare host have the
# same userland and the same kernel (the guest boots the host's own /boot/vmlinuz, extracted to an ELF vmlinux).
# The disk is a raw ext4 filesystem (no partition table) so both Cloud Hypervisor and Firecracker boot it as /dev/vda.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; . "$HERE/pins.env"
SIZE_G=${SIZE_G:-160}
[ "$(uname -r)" = "$HOST_KERNEL" ] || { echo "host kernel $(uname -r) != pinned $HOST_KERNEL" >&2; exit 1; }
[ ! -e /opt/drums ] || { echo "/opt/drums exists: this host is not pristine (run mkimage.sh before install.sh)" >&2; exit 1; }
mkdir -p "$T4"; cd "$T4"
# 1. the kernel: ELF vmlinux for PVH boot (both VMMs)
/usr/src/linux-headers-$HOST_KERNEL/scripts/extract-vmlinux /boot/vmlinuz-$HOST_KERNEL > vmlinux.tmp && mv vmlinux.tmp vmlinux
file vmlinux | grep -q 'ELF 64-bit' || { echo "vmlinux extraction failed" >&2; exit 1; }
# 2. the root filesystem
rm -f base.raw; truncate -s ${SIZE_G}G base.raw; mkfs.ext4 -q -L t4root -E lazy_itable_init=0 base.raw
M=$(mktemp -d); mount -o loop base.raw "$M"
rsync -aHAXx --numeric-ids --exclude=/var/lib/t4 --exclude=/tmp/'*' --exclude=/var/tmp/'*' --exclude=/var/lib/cloud/'*' \
      --exclude=/var/log/journal/'*' --exclude=/swap.img / "$M"/
# 3. guest configuration (identity, network and per-tenant values are set by mktenant.sh)
printf '/dev/vda / ext4 defaults,discard,errors=remount-ro 0 1\n' > "$M/etc/fstab"
touch "$M/etc/cloud/cloud-init.disabled"
rm -f "$M"/etc/netplan/*.yaml
: > "$M/etc/machine-id"; rm -f "$M/var/lib/dbus/machine-id"
rm -f "$M"/etc/ssh/ssh_host_*
# the host guard powers the HOST off; inside a guest it must not run (the host's guard covers the VM)
rm -f "$M/etc/systemd/system/multi-user.target.wants/drums-guard.service"
# regenerate ssh host keys on first boot
mkdir -p "$M/etc/systemd/system/multi-user.target.wants"
cat > "$M/etc/systemd/system/t4-hostkeys.service" <<'U'
[Unit]
Description=t4: generate ssh host keys on first boot
Before=ssh.service
ConditionPathExists=!/etc/ssh/ssh_host_ed25519_key
[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -A
[Install]
WantedBy=multi-user.target
U
ln -sf ../t4-hostkeys.service "$M/etc/systemd/system/multi-user.target.wants/t4-hostkeys.service"
sync; umount "$M"; rmdir "$M"
sha256sum vmlinux > vmlinux.sha256
echo "base.raw ${SIZE_G}G from $(hostname) kernel $HOST_KERNEL ami $HOST_AMI"
