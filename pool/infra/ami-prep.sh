#!/bin/bash
# Run on the builder after `install.sh --remote` succeeded: turn the installed host into a T3 host image (AMI).
# Leaves everything install.sh put on disk (image, verified marker, criu, drumsd, OSWorld checkers, configs, token),
# removes run-time state, and makes the Drums units start only from the first-boot script (host-firstboot.sh), which
# sizes the state tmpfs and restore permits for the instance it boots on.
set -euo pipefail
[ "$(id -u)" = 0 ] || { echo "run as root" >&2; exit 1; }
systemctl stop drumsd drums-origin drums-boot 2>/dev/null || true
systemctl disable -q drumsd drums-origin drums-boot
D=/var/lib/drums
# run-time state of this builder: slots, checkpoints, records of destroyed envs (the origin is provisioned again at first boot)
umount -l $D/state 2>/dev/null || true
PYTHONPATH=/opt/drums/src/drums-lab/benchmark python3 -m sharedos_rootfs.image detach --state $D/image-mount.json >/dev/null 2>&1 || true
umount -l /mnt/osworld 2>/dev/null || true
qemu-nbd --disconnect /dev/nbd0 >/dev/null 2>&1 || true
rm -rf $D/journal/* $D/forgotten $D/state/* $D/image-mount.json $D/bringup.json
rm -f /etc/drums-guard/hold /etc/drums-guard/config /run/drums-guard-idle-since
systemctl disable -q --now drums-guard-idle.timer 2>/dev/null || true
rm -f /home/ubuntu/.ssh/authorized_keys.bak
cloud-init clean --logs >/dev/null 2>&1 || true
journalctl --rotate >/dev/null 2>&1; journalctl --vacuum-time=1s >/dev/null 2>&1 || true
sync
echo "ami-prep done: $(cat /opt/drums/src/drumsd/RELEASE_COMMIT 2>/dev/null || git -C /opt/drums/src/drumsd rev-parse HEAD 2>/dev/null)"
