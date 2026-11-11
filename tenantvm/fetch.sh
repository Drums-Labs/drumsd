#!/usr/bin/env bash
# fetch.sh: download the pinned VMMs into $T4/bin and record their sha256 (first run writes bin/SHA256SUMS; later runs check it).
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; . "$HERE/pins.env"
mkdir -p "$T4/bin"; cd "$T4/bin"
[ -x cloud-hypervisor ] || { curl -fsSL -o cloud-hypervisor "$CH_URL"; chmod +x cloud-hypervisor; }
[ -x ch-remote ] || { curl -fsSL -o ch-remote "$CH_REMOTE_URL"; chmod +x ch-remote; }
if [ ! -x firecracker ]; then
  curl -fsSL -o fc.tgz "$FC_URL"; curl -fsSL -o fc.tgz.sha256 "$FC_URL.sha256.txt"
  echo "$(cut -d' ' -f1 fc.tgz.sha256)  fc.tgz" | sha256sum -c -
  tar xzf fc.tgz; cp release-$FC_VERSION-x86_64/firecracker-$FC_VERSION-x86_64 firecracker; chmod +x firecracker
fi
./cloud-hypervisor --version; ./firecracker --version | head -1
if [ -f "$HERE/bin.SHA256SUMS" ]; then sha256sum -c "$HERE/bin.SHA256SUMS"; else sha256sum cloud-hypervisor ch-remote firecracker | tee SHA256SUMS; fi
