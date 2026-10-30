#!/usr/bin/env bash
# test_boot.sh: drums_attach_image after a reboot (stale mount record, nbd still marked connected, nothing mounted) with
# shimmed findmnt/umount/qemu-nbd/python3: the leftovers are detached, the record removed, the attach verb run once.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
REALPY=$(command -v python3)
. "$HERE/boot.sh"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
D=$T; SYSBLOCK=$T/sys; mkdir -p "$SYSBLOCK/nbd0"; LOG=$T/calls; : > "$LOG"; export LOG REALPY SYSBLOCK
fail() { echo "FAIL: $*"; exit 1; }
mkdir -p "$T/bin"
printf '#!/bin/sh\nexit 1\n' > "$T/bin/findmnt"                       # nothing is mounted
printf '#!/bin/sh\necho "umount $*" >> "$LOG"\n' > "$T/bin/umount"
printf '#!/bin/sh\necho "qemu-nbd $*" >> "$LOG"; rm -f "$SYSBLOCK/nbd0/pid"\n' > "$T/bin/qemu-nbd"
cat > "$T/bin/python3" <<'SH'
#!/bin/sh
if [ "$1" = "-m" ]; then echo "attach $*" >> "$LOG"; [ ! -e "$5" ] || exit 1; exit 0; fi
exec "$REALPY" "$@"
SH
chmod +x "$T"/bin/*
export PATH="$T/bin:$PATH"
# stale record + device still marked connected
echo '{"nbd": "/dev/nbd0"}' > "$T/image-mount.json"; echo 1234 > "$SYSBLOCK/nbd0/pid"
drums_attach_image /img.qcow2 abc $T/mnt "$T/image-mount.json" /lab || fail "attach after reboot failed"
grep -q '^qemu-nbd --disconnect /dev/nbd0' "$LOG" || fail "leftover nbd not disconnected"
[ ! -e "$T/image-mount.json" ] || fail "stale record kept"
[ "$(grep -c '^attach' "$LOG")" = 1 ] || fail "attach verb not run exactly once"
! grep -q '^umount' "$LOG" || fail "unmounted something that was not mounted"
# clean host (no record, no device): no disconnect, attach runs
: > "$LOG"; rm -f "$SYSBLOCK/nbd0/pid"
drums_attach_image /img.qcow2 abc $T/mnt "$T/image-mount.json" /lab
! grep -q 'disconnect' "$LOG" || fail "disconnect on a clean host"
grep -q '^attach' "$LOG" || fail "no attach on a clean host"
# mounted with a live record: left alone
cat > "$T/bin/findmnt" <<'SH'
#!/bin/sh
exit 0
SH
: > "$LOG"; echo '{"nbd": "/dev/nbd0"}' > "$T/image-mount.json"; echo 1 > "$SYSBLOCK/nbd0/pid"
drums_attach_image /img.qcow2 abc $T/mnt "$T/image-mount.json" /lab
[ ! -s "$LOG" ] || fail "touched a live attachment: $(cat "$LOG")"
echo "test_boot ok"
