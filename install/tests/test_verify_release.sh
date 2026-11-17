#!/usr/bin/env bash
# verify-release.sh: checksum and signature checks of a release tarball. No root, no network: cosign is a fake on PATH
# that records its arguments and passes or fails on request.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"; V="$HERE/verify-release.sh"; fail=0
ok() { echo "PASS $*"; }; bad() { echo "FAIL $*"; fail=1; }
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }
mkdir -p "$T/bin" "$T/rel"
cat > "$T/bin/cosign" <<'FAKE'
#!/usr/bin/env bash
printf '%s\n' "$@" > "$FAKE_ARGS"
[ "${FAKE_COSIGN_FAIL:-0}" = 0 ] || { echo "error: none of the expected identities matched" >&2; exit 1; }
echo "Verified OK"
FAKE
chmod +x "$T/bin/cosign"
export FAKE_ARGS="$T/args"
TB="$T/rel/drums-release-0123456789ab.tar.gz"
echo "release bytes" > "$TB"
run() { "$@" > "$T/out" 2>&1 && echo 0 || echo $?; }
nocosign() { COSIGN="$T/no-such-cosign" "$@"; }

rc=$(run nocosign bash "$V" "$TB"); [ "$rc" = 1 ] && grep -q 'no drums-release-0123456789ab.tar.gz.sha256' "$T/out" && ok "a missing .sha256 refuses" || bad "missing sha: rc=$rc"
echo "0000000000000000000000000000000000000000000000000000000000000000  $(basename "$TB")" > "$TB.sha256"
rc=$(run nocosign bash "$V" "$TB"); [ "$rc" = 1 ] && grep -q 'sha256 mismatch' "$T/out" && ok "a different checksum refuses" || bad "mismatch: rc=$rc"
echo "not a hash" > "$TB.sha256"
rc=$(run nocosign bash "$V" "$TB"); [ "$rc" = 1 ] && ok "a malformed .sha256 refuses" || bad "malformed: rc=$rc"
echo "$(sha256 "$TB")  $(basename "$TB")" > "$TB.sha256"
rc=$(run nocosign bash "$V" "$TB"); [ "$rc" = 0 ] && grep -q 'sha256 ok' "$T/out" && grep -q 'WARNING: no .*sigstore.json' "$T/out" && ok "a matching checksum without a bundle passes with a warning" || bad "sha only: rc=$rc"
rc=$(run nocosign bash "$V" --require-signature "$TB"); [ "$rc" = 1 ] && ok "--require-signature refuses without a bundle" || bad "require, no bundle: rc=$rc"
echo '{}' > "$TB.sigstore.json"
rc=$(run nocosign bash "$V" --require-signature "$TB"); [ "$rc" = 1 ] && grep -q 'cosign is not installed' "$T/out" && ok "--require-signature refuses without cosign" || bad "require, no cosign: rc=$rc"
rc=$(run nocosign bash "$V" "$TB"); [ "$rc" = 0 ] && grep -q 'WARNING: cosign is not installed' "$T/out" && ok "a bundle without cosign passes with a warning" || bad "bundle, no cosign: rc=$rc"
rc=$(PATH="$T/bin:$PATH" run bash "$V" --require-signature "$TB"); [ "$rc" = 0 ] && grep -q 'signature ok' "$T/out" && ok "a verifying signature passes" || bad "good sig: rc=$rc $(cat "$T/out")"
grep -qx 'verify-blob' "$T/args" && grep -qx -- '--bundle' "$T/args" && grep -qx "$TB.sigstore.json" "$T/args" && ok "cosign verify-blob gets the bundle" || bad "args: $(cat "$T/args")"
grep -qx 'https://token.actions.githubusercontent.com' "$T/args" && ok "the OIDC issuer is pinned to GitHub Actions" || bad "issuer"
re=$(grep -A1 -x -- '--certificate-identity-regexp' "$T/args" | tail -1)
for good in "https://github.com/Drums-Labs/drumsd/.github/workflows/release.yml@refs/tags/v0.3.0"; do
  [[ "$good" =~ $re ]] && ok "the identity accepts this repo's release.yml on a v tag" || bad "identity rejects $good ($re)"; done
for evil in "https://github.com/Drums-Labs/drumsd/.github/workflows/ci.yml@refs/tags/v0.3.0" \
            "https://github.com/Drums-Labs/drumsd/.github/workflows/release.yml@refs/heads/main" \
            "https://github.com/someone/drumsd/.github/workflows/release.yml@refs/tags/v0.3.0" \
            "https://github.com/Drums-Labs/drumsdx/.github/workflows/release.yml@refs/tags/v0.3.0"; do
  [[ "$evil" =~ $re ]] && bad "identity accepts $evil" || ok "the identity refuses ${evil#https://github.com/}"; done
rc=$(PATH="$T/bin:$PATH" FAKE_COSIGN_FAIL=1 run bash "$V" "$TB"); [ "$rc" = 1 ] && grep -q 'signature does not verify' "$T/out" && grep -q 'expected identities' "$T/out" && ok "a signature that does not verify refuses (and shows cosign's reason)" || bad "bad sig: rc=$rc"
echo "tampered" >> "$TB"
rc=$(PATH="$T/bin:$PATH" run bash "$V" "$TB"); [ "$rc" = 1 ] && grep -q 'sha256 mismatch' "$T/out" && ok "a tampered tarball refuses before cosign runs" || bad "tampered: rc=$rc"
rc=$(run bash "$V"); [ "$rc" = 2 ] && ok "no argument is a usage error" || bad "usage rc=$rc"
# install.sh wires it in: verifies the tarball next to the unpacked dir and passes --require-signature through
I="$HERE/install.sh"
grep -q -- '--require-signature) REQSIG=1' "$I" && ok "install.sh takes --require-signature" || bad "install.sh flag"
grep -q 'install/verify-release.sh' "$I" && ok "install.sh calls verify-release.sh" || bad "install.sh call"
exit $fail
