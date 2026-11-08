#!/bin/bash
# Coordinator host setup (Ubuntu 24.04; the T3 AMI or a stock one): drums-pool from this checkout, the drums SDK,
# Prometheus + Alertmanager + Grafana at pinned versions (sha256 checked against pool/deploy/obs-lock.json).
#   sudo bash pool/run/coordinator-setup.sh /path/to/drums-release-<commit>.tar.gz
set -euo pipefail
REPO="$(cd "$(dirname "$0")/../.." && pwd)"
REL="${1:?release tarball (for the SDK wheel)}"
O=/opt/drums-pool; mkdir -p $O/bin /etc/drums-pool /var/lib/drums-pool; chown -R "${SUDO_USER:-ubuntu}" /etc/drums-pool /var/lib/drums-pool
# Rust at the workspace's pinned toolchain (install.sh pins the same one)
RUST=$(python3 -c "import json; print(json.load(open('$REPO/install/lock.json'))['rust_toolchain'])" 2>/dev/null || echo 1.96.1)
# the Drums image already has the pinned toolchain under /opt/drums/rust
[ -x /opt/drums/rust/bin/cargo ] && export RUSTUP_HOME=/opt/drums/rust CARGO_HOME=/opt/drums/rust PATH=/opt/drums/rust/bin:$PATH
if ! command -v cargo >/dev/null || ! cargo --version | grep -q "$RUST"; then
  export RUSTUP_HOME=/opt/rustup CARGO_HOME=/opt/cargo
  [ -x /opt/cargo/bin/cargo ] || curl -sSf https://sh.rustup.rs | sh -s -- -y -q --profile minimal --default-toolchain "$RUST" >/dev/null
  export PATH=/opt/cargo/bin:$PATH
fi
apt-get install -y -q build-essential pkg-config >/dev/null 2>&1 || true
(cd "$REPO" && cargo build -q --release --locked -p drums-pool)
install -m 0755 "$REPO/target/release/drums-pool" $O/bin/drums-pool
# SDK wheel from the release (the same wheel customers get)
T=$(mktemp -d); tar -xzf "$REL" -C "$T"; W=$(ls "$T"/*/sdk/*.whl | head -1)
python3 -m pip install -q --break-system-packages --force-reinstall "$W"
python3 -c "import drums; print('sdk', drums.__file__)"
# observability, pinned
LOCK="$REPO/pool/deploy/obs-lock.json"
fetch() { # name url sha256 dest
  local f=$O/dl/$(basename "$2"); mkdir -p $O/dl
  [ -s "$f" ] || curl -sSfL -o "$f" "$2"
  echo "$3  $f" | sha256sum -c --quiet - || { echo "sha256 mismatch for $1 ($f)" >&2; exit 1; }
  mkdir -p "$4"; tar -xzf "$f" -C "$4" --strip-components=1
}
for n in prometheus alertmanager grafana; do
  url=$(python3 -c "import json; print(json.load(open('$LOCK'))['$n']['url'])")
  sha=$(python3 -c "import json; print(json.load(open('$LOCK'))['$n']['sha256'])")
  fetch $n "$url" "$sha" $O/$n
done
echo "coordinator ready: $($O/bin/drums-pool --version); $($O/prometheus/prometheus --version 2>&1 | head -1)"
