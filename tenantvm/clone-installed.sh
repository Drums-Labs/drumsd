#!/usr/bin/env bash
# clone-installed.sh SRCNAME: stop tenant SRCNAME, copy its (drums-installed) disk to $T4/installed.raw as the template
# every tenant is then made from. Run after drums is installed inside SRCNAME.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; . "$HERE/pins.env"
S=${1:?src name}
if [ -S "$T4/$S.ch.sock" ]; then "$T4/bin/ch-remote" --api-socket "$T4/$S.ch.sock" shutdown-vmm 2>/dev/null || true; fi
systemctl stop "t4vm-$S" 2>/dev/null || true; sleep 3
cp --sparse=always "$T4/$S.raw" "$T4/installed.raw"
echo "$T4/installed.raw is the installed template ($(du -h --apparent-size $T4/installed.raw | cut -f1))"
