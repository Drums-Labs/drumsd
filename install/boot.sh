#!/usr/bin/env bash
# Drums boot (installed as /opt/drums/boot.sh by install.sh; run by drums-boot.service and drums-origin.service).
#
#   boot.sh prepare   before drumsd: mount the state tmpfs, load nbd/overlay, re-attach the verified image read-only
#   boot.sh origin    after the host is up: re-provision the clean checkpoint with bringup.py (what install.sh step 10 runs)
#
# The state dir is a tmpfs and the image mount is a qemu-nbd attachment: a reboot (EC2 stop/start) removes both, and
# drumsd's journal on disk still lists the checkpoint. This puts them back from what install.sh left on disk
# (/etc/drums/boot.env, $D/image/Ubuntu.qcow2 and its verified marker, the pinned sources); nothing is downloaded or built.
# Every failure stops with a message and a non-zero exit, so the unit is failed and drumsd's health shows the checkpoint
# as lost (state_lost) - never a half-restored host that looks fine.
# install.sh sources this file for drums_mount_state and drums_attach_image (the main part runs only when executed).

D=/var/lib/drums; O=/opt/drums; E=/etc/drums; SYSBLOCK=${DRUMS_SYS_BLOCK:-/sys/block}
declare -F say >/dev/null || say() { echo "$(date -u +%FT%TZ) [drums-boot] $*"; }
declare -F die >/dev/null || die() { echo "$(date -u +%FT%TZ) [drums-boot] FAILED: $*" >&2; exit 1; }

drums_mount_state() { # SIZE_GIB
  mkdir -p "$D/state" "$D/journal"
  findmnt -n "$D/state" >/dev/null || mount -t tmpfs -o "size=${1}G,mode=0700" tmpfs "$D/state" || die "mount state tmpfs"
  chmod 0700 "$D/state"
}

# Attach IMAGE read-only at MP, leaving $STATE (the mount record) describing exactly that. A record or mount left by an
# earlier boot is cleaned first: record without a live mount -> detach what is left of the nbd device, remove the record.
drums_attach_image() { # IMAGE SHA256 MP STATE LAB
  local img=$1 sha=$2 mp=$3 state=$4 lab=$5 nbd=/dev/nbd0 n
  if [ -s "$state" ]; then
    n=$(python3 -c "import json,sys; print(json.load(open(sys.argv[1])).get('nbd',''))" "$state" 2>/dev/null || true)
    [ -z "$n" ] || nbd=$n
  fi
  local dev=${nbd##*/}
  if findmnt -n "$mp" >/dev/null; then
    if [ -s "$state" ] && [ -e "$SYSBLOCK/$dev/pid" ]; then say "image already attached at $mp"; return 0; fi
    say "$mp is mounted without a live attach record: unmounting it"
    umount "$mp" || die "cannot unmount $mp (in use?); not touching $nbd"
  fi
  if [ -e "$state" ]; then say "removing stale image mount record $state (the mount is gone)"; rm -f "$state"; fi
  if [ -e "$SYSBLOCK/$dev/pid" ]; then
    say "disconnecting $nbd left over from before the restart"
    qemu-nbd --disconnect "$nbd" >/dev/null || die "cannot disconnect $nbd"
    for _ in $(seq 1 40); do [ -e "$SYSBLOCK/$dev/pid" ] || break; sleep 0.5; done
    [ ! -e "$SYSBLOCK/$dev/pid" ] || die "$nbd is still connected after disconnect"
  fi
  mkdir -p "$mp"
  PYTHONPATH="$lab" PYTHONDONTWRITEBYTECODE=1 python3 -m sharedos_rootfs.image attach --image "$img" --expected-sha256 "$sha" \
    --mountpoint "$mp" --state "$state" --nbd "$nbd" > "$D/image-attach.log" 2>&1 || { cat "$D/image-attach.log"; die "image attach"; }
}

drums_boot_main() {
  set -euo pipefail
  [ "$(id -u)" = 0 ] || die "run as root"
  [ -r "$E/boot.env" ] || die "$E/boot.env missing: install.sh has not completed on this host"
  . "$E/boot.env"     # TMPFS_GIB IMAGE_SHA256 IMAGE_ID
  local lab="$O/src/drums-lab/benchmark" q="$D/image/Ubuntu.qcow2"
  case "${1:-}" in
    prepare)
      drums_mount_state "$TMPFS_GIB"
      modprobe nbd max_part=16 || die "modprobe nbd"
      modprobe overlay || die "modprobe overlay"
      # the same marker install.sh step 4 wrote after verifying the sha256 (no re-download, no rehash of 24 GB)
      { [ -f "$q" ] && [ -f "$D/image/qcow2.verified" ] && [ "$(cat "$D/image/qcow2.verified")" = "$IMAGE_SHA256 $(stat -c %s.%Y "$q")" ]; } \
        || die "$q is missing or does not match its verified marker: run the installer again"
      drums_attach_image "$q" "$IMAGE_SHA256" /mnt/osworld "$D/image-mount.json" "$lab"
      findmnt -n /mnt/osworld >/dev/null || die "/mnt/osworld is not mounted after attach"
      say "prepared: state tmpfs ${TMPFS_GIB} GiB, image mounted read-only at /mnt/osworld"
      ;;
    origin)
      { findmnt -n "$D/state" >/dev/null && findmnt -n /mnt/osworld >/dev/null; } || die "state tmpfs or /mnt/osworld not mounted: boot.sh prepare did not run"
      # the same command line as install.sh step 10: adopts the checkpoint if drumsd has it intact, otherwise deletes the
      # stale/lost origin and provisions slot 0 again from the image
      python3 "$O/src/drumsd/install/bringup.py" --criu-dir "$O/criu/bin" --lab "$lab" --rootfs-config "$E/rootfs-config.json" \
        --drumsd-config "$E/drumsd.json" --image-id "$IMAGE_ID" --record "$D/bringup.json" || die "bring-up of the clean checkpoint"
      say "origin $IMAGE_ID is serving"
      ;;
    *) die "usage: boot.sh prepare|origin";;
  esac
}

if [ "${BASH_SOURCE[0]}" = "$0" ]; then drums_boot_main "$@"; fi
