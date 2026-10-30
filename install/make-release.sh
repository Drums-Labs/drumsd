#!/usr/bin/env bash
# make-release.sh OUT_DIR  -- on a machine with the three repos: a self-contained release for install.sh.
#   DRUMS_CRIU=path/to/drums-criu (branch app-compat)  DRUMS_LAB=path/to/drums-lab
# The release holds install.sh, install/ (lock, hashed Python lock, bring-up), git bundles of drumsd (this commit),
# drums-criu and drums-lab (the commits pinned in install/lock.json), RELEASE_COMMIT and SHA256SUMS.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$(mkdir -p "${1:?out dir}" && cd "$1" && pwd)"
CRIU="${DRUMS_CRIU:?set DRUMS_CRIU}"; LAB="${DRUMS_LAB:?set DRUMS_LAB}"
[ -z "$(git -C "$HERE" status --porcelain --untracked-files=no)" ] || { echo "drumsd tree has uncommitted changes" >&2; exit 1; }
C=$(git -C "$HERE" rev-parse HEAD)
lk() { python3 -c "import json; print(json.load(open('$HERE/install/lock.json'))['sources']['$1']['commit'])"; }
for pair in "$CRIU:$(lk drums-criu)" "$LAB:$(lk drums-lab)"; do
  git -C "${pair%%:*}" cat-file -e "${pair#*:}^{commit}" || { echo "${pair%%:*} lacks ${pair#*:}" >&2; exit 1; }
done
R="$OUT/drums-release-${C:0:12}"
rm -rf "$R"; mkdir -p "$R/sources"
cp -r "$HERE/install" "$R/install"; cp "$HERE/install/install.sh" "$R/install.sh"
echo "$C" > "$R/RELEASE_COMMIT"
bundle() { # bundle REPO NAME COMMIT
  git -C "$1" branch -f "release-$2" "$3" >/dev/null
  git -C "$1" bundle create -q "$R/sources/$2.bundle" "release-$2"
  git -C "$1" branch -D "release-$2" >/dev/null
}
bundle "$HERE" drumsd "$C"
bundle "$CRIU" drums-criu "$(lk drums-criu)"
bundle "$LAB" drums-lab "$(lk drums-lab)"
(cd "$R" && find . -type f ! -name SHA256SUMS | sort | xargs sha256sum > SHA256SUMS)
COPYFILE_DISABLE=1 tar --no-mac-metadata --no-xattrs -C "$OUT" -czf "$R.tar.gz" "$(basename "$R")"
sha256sum "$R.tar.gz"
