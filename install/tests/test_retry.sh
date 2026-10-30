#!/usr/bin/env bash
# test_retry.sh: a shimmed apt-get that fails twice (503 on InRelease, exit 0 as apt does; then a hard error), then works.
set -euo pipefail     # as install.sh
HERE="$(cd "$(dirname "$0")/.." && pwd)"
. "$HERE/retry.sh"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
export RETRY_LOG_DIR=$T DRUMS_RETRY_BASE_S=0 DRUMS_RETRY_MAX_S=0
cat > "$T/apt-get" <<'SH'
#!/usr/bin/env bash
c=$(cat "$CNT" 2>/dev/null || echo 0); c=$((c + 1)); echo $c > "$CNT"
case $c in
  1) echo "W: Failed to fetch https://snapshot.ubuntu.com/ubuntu/x/dists/noble/InRelease  503  Service Unavailable"; exit 0 ;;
  2) echo "E: Failed to fetch http://mirror/Packages.xz  File has unexpected size"; exit 100 ;;
  *) echo "Reading package lists..."; exit 0 ;;
esac
SH
chmod +x "$T/apt-get"
fail() { echo "FAIL: $*"; exit 1; }
export CNT=$T/count
out=$(PATH="$T:$PATH" retry "apt update" apt-get update) || fail "retry gave up"
[ "$(cat "$CNT")" = 3 ] || fail "expected 3 calls, got $(cat "$CNT")"
echo "$out" | grep -q "attempt 1 of 5 failed" || fail "attempt 1 not logged"
echo "$out" | grep -q "attempt 2 of 5 failed (exit 100)" || fail "attempt 2 not logged"
echo "$out" | grep -q "succeeded on attempt 3" || fail "success not logged"
# bounded: a mirror that never works gives up after DRUMS_RETRY_ATTEMPTS
echo 99 > "$CNT"; cat > "$T/apt-get" <<'SH'
#!/usr/bin/env bash
echo "E: down"; exit 100
SH
DRUMS_RETRY_ATTEMPTS=3 PATH="$T:$PATH" retry "apt update" apt-get update > "$T/o" && fail "should have given up"
[ "$(grep -c 'failed (exit 100)' "$T/o")" = 3 ] || fail "expected 3 logged failures"
ls "$T"/retry-apt_update-*.log > /dev/null || fail "attempt outputs not kept"
echo "PASS test_retry"
