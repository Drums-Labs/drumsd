#!/usr/bin/env bash
# verify-release.sh [--require-signature] drums-release-<commit12>.tar.gz
#
# Checks a downloaded drumsd release before anything in it runs:
#   1. <tarball>.sha256 must sit next to the tarball and match it (a missing or different checksum refuses);
#   2. <tarball>.sigstore.json, when present, is checked with `cosign verify-blob`: the signing certificate must come from
#      GitHub's OIDC issuer for this repository's .github/workflows/release.yml on a v* tag (Sigstore keyless; the
#      entry is in the public Rekor transparency log). A bundle that does not verify refuses.
# Without --require-signature a missing bundle or a missing cosign is a warning; with it, both refuse.
# Exit 0 = verified, 1 = refused, 2 = usage. Needs no root and no network except cosign's Rekor/TUF lookups.
# DRUMS_RELEASE_REPO (default Drums-Labs/drumsd) names the repository a fork's releases are signed from.
set -euo pipefail
REQ=0; TB=""
while [ $# -gt 0 ]; do
  case "$1" in
    --require-signature) REQ=1; shift;;
    -h|--help) sed -n 2,11p "$0"; exit 0;;
    -*) echo "unknown option $1" >&2; exit 2;;
    *) [ -z "$TB" ] || { echo "one tarball only" >&2; exit 2; }; TB="$1"; shift;;
  esac
done
[ -n "$TB" ] || { echo "usage: verify-release.sh [--require-signature] drums-release-<commit12>.tar.gz" >&2; exit 2; }
say() { echo "[verify-release] $*"; }
refuse() { echo "[verify-release] REFUSED: $*" >&2; exit 1; }
[ -f "$TB" ] || refuse "no such file: $TB"
REPO="${DRUMS_RELEASE_REPO:-Drums-Labs/drumsd}"
COSIGN="${COSIGN:-cosign}"
ISSUER=https://token.actions.githubusercontent.com
# the certificate's SAN: the workflow file at a release tag of this repository
IDENTITY="^https://github\\.com/${REPO//./\\.}/\\.github/workflows/release\\.yml@refs/tags/v[0-9][^/]*\$"

sha256() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }

# 1. checksum
[ -f "$TB.sha256" ] || refuse "no $(basename "$TB").sha256 next to the tarball (download it from the same release)"
want="$(awk 'NR==1{print tolower($1)}' "$TB.sha256")"
[[ "$want" =~ ^[0-9a-f]{64}$ ]] || refuse "$(basename "$TB").sha256 does not hold a sha256"
have="$(sha256 "$TB")"
[ "$have" = "$want" ] || refuse "sha256 mismatch: the tarball is $have, the .sha256 says $want"
say "sha256 ok: $have"

# 2. signature
B="$TB.sigstore.json"
if [ ! -f "$B" ]; then
  [ "$REQ" = 0 ] || refuse "no $(basename "$B") next to the tarball and --require-signature was given"
  say "WARNING: no $(basename "$B"); the checksum proves the download is intact, not who built it"
  exit 0
fi
if ! command -v "$COSIGN" >/dev/null 2>&1; then
  [ "$REQ" = 0 ] || refuse "cosign is not installed and --require-signature was given (https://docs.sigstore.dev/cosign/system_config/installation/)"
  say "WARNING: cosign is not installed; the signature in $(basename "$B") was not checked"
  exit 0
fi
if ! out="$("$COSIGN" verify-blob --bundle "$B" --certificate-identity-regexp "$IDENTITY" --certificate-oidc-issuer "$ISSUER" "$TB" 2>&1)"; then
  printf '%s\n' "$out" | tail -5 >&2
  refuse "signature does not verify for $REPO release.yml on a v* tag (cosign verify-blob failed)"
fi
say "signature ok: signed by $REPO .github/workflows/release.yml on a v* tag ($ISSUER)"
