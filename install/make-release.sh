#!/usr/bin/env bash
# make-release.sh OUT_DIR  -- on a machine with the four repos: a self-contained release for install.sh.
#   DRUMS_CRIU=path/to/drums-criu  DRUMS_SDK=path/to/drums-python-sdk (Drums-Labs/drums-python-sdk)
# The release holds install.sh, install/ (lock, hashed Python lock, bring-up), python/ (the runtime's Python side),
# git bundles of drumsd (this commit) and drums-criu (the commit pinned in install/lock.json), the SDK wheel (sdk/drums*.whl, built from the
# Drums-Labs/drums-python-sdk commit pinned in install/lock.json), the user docs
# (README.md, CHANGELOG.md, docs/ incl. docs/install.md and docs/release-notes/, examples/, api/openapi.json), RELEASE_COMMIT and SHA256SUMS.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$(mkdir -p "${1:?out dir}" && cd "$1" && pwd)"
CRIU="${DRUMS_CRIU:?set DRUMS_CRIU}"; SDK="${DRUMS_SDK:?set DRUMS_SDK}"
[ -z "$(git -C "$HERE" status --porcelain --untracked-files=no)" ] || { echo "drumsd tree has uncommitted changes" >&2; exit 1; }
C=$(git -C "$HERE" rev-parse HEAD)
lk() { python3 -c "import json; print(json.load(open('$HERE/install/lock.json'))['sources']['$1']['commit'])"; }
for pair in "$CRIU:$(lk drums-criu)" "$SDK:$(lk drums-sdk)"; do
  git -C "${pair%%:*}" cat-file -e "${pair#*:}^{commit}" || { echo "${pair%%:*} lacks ${pair#*:}" >&2; exit 1; }
done
R="$OUT/drums-release-${C:0:12}"
rm -rf "$R"; mkdir -p "$R/sources"
# only the committed files of install/ (an untracked file such as a __pycache__ of a test run must not ship)
git -C "$HERE" archive --format=tar HEAD install python | tar -x -C "$R"; cp "$R/install/install.sh" "$R/install.sh"
# the user docs, from the same commit: a customer reads these, not the wheel's source
git -C "$HERE" archive --format=tar HEAD CHANGELOG.md docs examples api/openapi.json verifiers | tar -x -C "$R"
mv "$R/docs/README.md" "$R/README.md"
for f in README.md docs/install.md CHANGELOG.md python/sharedos_rootfs/__init__.py python/README.md docs/host-requirements.md docs/actions-and-observations.md docs/validity.md docs/lifecycle.md docs/custom-tasks.md docs/security.md docs/procview.md docs/architecture.md docs/reward-audit.md docs/release-notes/rc1.md docs/design/verifier-sandbox.md api/openapi.json verifiers/drums_t6.py; do
  [ -s "$R/$f" ] || { echo "release is missing $f" >&2; exit 1; }
done
# the shipped lab verifiers hash to what the lock approves (the installer checks the same from the drumsd bundle)
python3 - "$R" <<'PY'
import hashlib, json, sys
r = sys.argv[1]
for name, v in json.load(open(f"{r}/install/lock.json"))["verifiers"].items():
    if not isinstance(v, dict):
        continue
    got = hashlib.sha256(open(f"{r}/{v['path']}", "rb").read()).hexdigest()
    if got != v["sha256"]:
        sys.exit(f"lab verifier {name}: {v['path']} is {got}, install/lock.json says {v['sha256']}")
PY
echo "$C" > "$R/RELEASE_COMMIT"
bundle() { # bundle REPO NAME COMMIT
  git -C "$1" branch -f "release-$2" "$3" >/dev/null
  git -C "$1" bundle create -q "$R/sources/$2.bundle" "release-$2"
  git -C "$1" branch -D "release-$2" >/dev/null
}
bundle "$HERE" drumsd "$C"
bundle "$CRIU" drums-criu "$(lk drums-criu)"
# the wheel from the pinned SDK commit, not the clone's working tree
S=$(mktemp -d); trap 'rm -rf "$S"' EXIT
git -C "$SDK" archive --format=tar "$(lk drums-sdk)" | tar -x -C "$S"
python3 "$HERE/install/build_wheel.py" "$S" "$R/sdk" > /dev/null
(cd "$R" && find . -type f ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS)
# no xattrs or macOS metadata in the tarball: bsdtar (macOS) needs --no-mac-metadata (and COPYFILE_DISABLE), which GNU tar
# (Linux) does not know; GNU tar stores no xattrs unless asked and takes --no-xattrs.
if tar --version 2>/dev/null | grep -q 'GNU tar'; then TARX=(--no-xattrs); else TARX=(--no-mac-metadata --no-xattrs); fi
COPYFILE_DISABLE=1 tar "${TARX[@]}" -C "$OUT" -czf "$R.tar.gz" "$(basename "$R")"
sha256sum "$R.tar.gz"
