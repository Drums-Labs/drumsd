#!/usr/bin/env bash
# test_libc_follow.sh: libc6-dev follows the host's libc6 when the host is newer than the apt snapshot (the stock
# Ubuntu 24.04 AMI of 2026-11-20 ships libc6 2.39-0ubuntu8.12; snapshot 20261029T000000Z has libc6-dev 2.39-0ubuntu8.9).
# dpkg-query, apt-cache and apt-get are shims; dpkg --compare-versions is the real one when present.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
. "$HERE/retry.sh"; . "$HERE/libc-follow.sh"
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
export RETRY_LOG_DIR=$T DRUMS_RETRY_ATTEMPTS=1 DRUMS_RETRY_BASE_S=0 DRUMS_RETRY_MAX_S=0 CALLS=$T/calls
say() { echo "$*" >> "$T/said"; }
fail() { echo "FAIL: $*"; exit 1; }
ok() { echo "PASS $*"; }
cat > "$T/dpkg-query" <<'SH'
#!/usr/bin/env bash
case "$*" in
  *libc6-dev*) [ -n "${HAVE_DEV:-}" ] && { printf '%s' "$HAVE_DEV"; exit 0; } || exit 1 ;;
  *libc6*) printf '%s' "$HOST" ;;
esac
SH
cat > "$T/apt-cache" <<'SH'
#!/usr/bin/env bash
echo " libc6-dev | $SNAPV | https://snapshot.ubuntu.com/ubuntu/x noble-updates/main amd64 Packages"
SH
cat > "$T/apt-get" <<'SH'
#!/usr/bin/env bash
echo "$*" >> "$CALLS"
case "$*" in *"libc6-dev=$HOST"*) [ "${EXACT_GONE:-}" = 1 ] && { echo "E: Version '$HOST' for 'libc6-dev' was not found"; exit 100; } ;; esac
exit 0
SH
if ! command -v dpkg > /dev/null; then   # macOS: a dpkg that compares the two Ubuntu revisions used here
  cat > "$T/dpkg" <<'SH'
#!/usr/bin/env bash
a=${2##*ubuntu}; b=${4##*ubuntu}; [ "$(printf '%s\n%s\n' "$a" "$b" | sort -V | head -1)" = "$a" ]
SH
fi
chmod +x "$T"/*
run() { rm -f "$CALLS" "$T/said"; PATH="$T:$PATH" libc_dev_follow 20261029T000000Z; }

HOST=2.39-0ubuntu8.9 SNAPV=2.39-0ubuntu8.9 run || fail "same version"
[ ! -s "$CALLS" ] && ok "host libc6 = snapshot: no extra apt call, the snapshot supplies libc6-dev" || fail "apt called: $(cat "$CALLS")"

export HOST=2.39-0ubuntu8.12 SNAPV=2.39-0ubuntu8.9
run || fail "newer host"
grep -q '^update -q$' "$CALLS" && grep -q 'install -y -q --no-install-recommends libc6-dev=2.39-0ubuntu8.12 libc-dev-bin=2.39-0ubuntu8.12' "$CALLS" \
  && grep -q 'update -q --snapshot 20261029T000000Z' "$CALLS" && grep -q 'newer than snapshot' "$T/said" \
  && ok "host libc6 newer: libc6-dev and libc-dev-bin at exactly the host's version, then the snapshot lists again" || fail "$(cat "$CALLS")"

EXACT_GONE=1 run || fail "fallback"
grep -q 'install -y -q --no-install-recommends libc6 libc-bin libc6-dev libc-dev-bin' "$CALLS" && grep -q 'no longer has' "$T/said" \
  && ok "the archive moved on: its current libc6 and libc6-dev together, said out loud" || fail "$(cat "$CALLS")"

HAVE_DEV="ii 2.39-0ubuntu8.12" run || fail "rerun"
[ ! -s "$CALLS" ] && ok "a rerun with libc6-dev already matching does nothing" || fail "apt called on rerun"
