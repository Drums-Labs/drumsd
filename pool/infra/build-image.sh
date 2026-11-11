#!/bin/bash
# On a fresh Ubuntu 24.04 builder: install the release, add the T3 scripts and the coordinator key, prepare for CreateImage.
#   bash build-image.sh RELEASE.tar.gz COORDINATOR.pub      (then CreateImage with NoReboot=true)
set -euo pipefail
REL="${1:?release tarball}"; PUB="${2:?coordinator public key file}"
HERE="$(cd "$(dirname "$0")" && pwd)"
W=$(mktemp -d); tar -xzf "$REL" -C "$W"; cd "$W"/drums-release-*
sha256sum -c --quiet SHA256SUMS
sudo ./install.sh --remote
sudo install -d /opt/drums-t3
sudo install -m 0755 "$HERE/guard.sh" "$HERE/host-firstboot.sh" "$HERE/ami-prep.sh" /opt/drums-t3/
sudo install -m 0644 "$PUB" /opt/drums-t3/coordinator.pub
grep -qF "$(cat "$PUB")" ~/.ssh/authorized_keys || cat "$PUB" >> ~/.ssh/authorized_keys
sudo cp /var/lib/drums/install-receipt.json /tmp/install-receipt.json; sudo chmod 644 /tmp/install-receipt.json
sudo bash /opt/drums-t3/ami-prep.sh
echo BUILD-IMAGE-DONE
