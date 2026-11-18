#!/usr/bin/env bash
# install.sh, one pass: the release tarball is verified (checksum, signature with --require-signature) before anything
# runs, and the same run installs the hash-pinned lab verifier, seeds the verifier store and creates the drums-verify
# user the sandbox runs checkers as. A static check of install.sh's order plus the lock's pin (the install itself
# needs root and a fresh host: CONTRIBUTING.md#releases).
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"; ROOT="$(dirname "$HERE")"; I="$HERE/install.sh"; fail=0
ok() { echo "PASS $*"; }; bad() { echo "FAIL $*"; fail=1; }
line() { grep -n -m1 -F -- "$1" "$I" | cut -d: -f1; }
v=$(line 'install/verify-release.sh"); [ "$REQSIG" = 0 ] || VR+=(--require-signature)')
s=$(line 'sha256sum -c --quiet SHA256SUMS')
lab=$(line 'sha256sum -c --quiet - || die "lab verifier')
store=$(line '"/var/lib/drums/verifiers/$VSHA.py"')
user=$(line 'useradd --system --no-create-home --home-dir /nonexistent --shell /usr/sbin/nologin drums-verify')
cfg=$(line '"verifiers": {"store_dir": "/var/lib/drums/verifiers", "uid": $VER_UID, "gid": $VER_GID}')
for n in v s lab store user cfg; do [ -n "${!n}" ] || bad "install.sh is missing step $n"; done
[ "$fail" = 0 ] || exit 1
[ "$s" -lt "$v" ] && [ "$v" -lt "$lab" ] && ok "the tarball is verified before any release file is installed" || bad "order: verify $v, lab $lab"
[ "$lab" -lt "$store" ] && ok "the lab verifier is checked against the lock before it is installed and seeded" || bad "order: check $lab, seed $store"
[ "$user" -lt "$cfg" ] && ok "drums-verify exists before drumsd.json names its uid" || bad "order: user $user, config $cfg"
# the lock pins the file the release ships, and the release refuses to build otherwise
python3 - "$ROOT" <<'PY' && ok "install/lock.json pins verifiers/drums_t6.py by its sha256" || bad "lock and verifiers/ differ"
import hashlib, json, sys
r = sys.argv[1]
v = json.load(open(f"{r}/install/lock.json"))["verifiers"]["drums_t6"]
assert hashlib.sha256(open(f"{r}/{v['path']}", "rb").read()).hexdigest() == v["sha256"]
PY
grep -q 'api/openapi.json verifiers |' "$HERE/make-release.sh" && grep -q 'lab verifier {name}' "$HERE/make-release.sh" \
  && ok "make-release.sh ships verifiers/ and checks it against the lock" || bad "make-release.sh"
exit $fail
