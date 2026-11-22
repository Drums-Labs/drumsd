#!/usr/bin/env python3
"""check-release.py drums-release-<commit12>.tar.gz  -- refuse a release tarball that carries anything it must not.

make-release.sh runs this on the tarball it just wrote and deletes the tarball when the check fails; release.yml runs
it again before anything is uploaded. Exit 0 = clean, 1 = refused (every reason is printed), 2 = usage.

What a release may hold (and nothing else):
  * one top directory drums-release-<12 hex>/ with only these entries: install.sh, install/, python/, README.md,
    CHANGELOG.md, docs/, examples/, api/, verifiers/, sources/, sdk/, LICENSE, NOTICE, THIRD_PARTY_LICENSES,
    RELEASE_COMMIT, SHA256SUMS (LICENSE, NOTICE and THIRD_PARTY_LICENSES are required);
  * sources/ holds exactly drumsd.bundle and drums-criu.bundle. Any other bundle refuses: v0.2.1 shipped a private
    repository's full history as sources/drums-lab.bundle, and this check exists so that can't happen again;
  * each bundle has exactly one head, refs/heads/release-<name>. The drumsd bundle's head is RELEASE_COMMIT and its
    root is drumsd's root commit; the drums-criu bundle contains upstream CRIU v4.3, the fork's base. A bundle of
    another repository under one of these names refuses;
  * sdk/ holds exactly one py3-none-any wheel;
  * only regular files and directories (no links, devices, absolute or .. paths), and SHA256SUMS matches.

Content checks, on every file in the tarball and every file inside the wheel:
  * no private key blocks, AWS access key ids, GitHub tokens or Drums API keys;
  * no macOS home paths, no citations of the internal doc store, no internal ssh host aliases;
  * no 12-digit number whose sha256 is a known Drums AWS account id (the ids themselves are not written here).
And on the history in each bundle (drumsd: all of it; drums-criu: the fork's commits after v4.3): no private key
blocks, AWS access key ids or known AWS account ids.

DRUMS_RELEASE_DENYLIST, when set, names a file of extra regular expressions (one per line, # comments) applied to
the files; keep private patterns there, outside the repository.
"""
from __future__ import annotations

import hashlib
import io
import os
import re
import subprocess
import sys
import tarfile
import tempfile
import zipfile

TOP_ALLOWED = {"install.sh", "install", "python", "README.md", "CHANGELOG.md", "docs", "examples", "api", "verifiers",
               "sources", "sdk", "LICENSE", "NOTICE", "THIRD_PARTY_LICENSES", "RELEASE_COMMIT", "SHA256SUMS"}
TOP_REQUIRED = {"install.sh", "install", "python", "LICENSE", "NOTICE", "THIRD_PARTY_LICENSES", "RELEASE_COMMIT",
                "SHA256SUMS", "sources", "sdk"}
BUNDLES = {"drumsd.bundle", "drums-criu.bundle"}
DRUMSD_ROOT = "c759e11cddcc20e11142717a209ecfecc016e17e"   # drumsd's first commit
CRIU_BASE = "2b7ac603cda31d015d91c856c34c557cb2300e7d"     # upstream CRIU v4.3, the fork's base
# sha256 of the decimal AWS account ids the Drums infrastructure uses; a match anywhere refuses
ACCOUNT_HASHES = {
    "3e93f9a2b85405878f732c8fdb31564f9bac9cf2e29c67c046e5c111bfbbdbe3",
    "4e74000b065475ee7061bb5aef8b58b01b1de074bdb6bcbf616dca81290859bd",
}
SECRET_PATTERNS = [
    ("private key block", re.compile(rb"-----BEGIN [A-Z0-9 ]*PRIVATE KEY-----")),
    ("AWS access key id", re.compile(rb"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b")),
    ("GitHub token", re.compile(rb"\b(?:ghp|gho|ghu|ghs|ghr)_[A-Za-z0-9]{36}\b|\bgithub_pat_[A-Za-z0-9_]{40,}")),
    ("Drums API key", re.compile(rb"\bdrums_sk_[A-Za-z0-9]{24,}")),
]
# the character classes ([U], [i], [a]) keep this file from matching its own patterns
FILE_PATTERNS = SECRET_PATTERNS + [
    ("macOS home path", re.compile(rb"/[U]sers/[A-Za-z][A-Za-z0-9._-]*/")),
    ("internal doc store citation", re.compile(rb"\bdrums-[i]nfo\b")),
    ("internal ssh host alias", re.compile(rb"\b(?:drums-l[i]nux(?:-big)?|drums-m[a]c|stud[i]o-[ab])\b")),
    ("private archive repository", re.compile(rb"\bdrums-research-h[i]story\b")),
]
TWELVE = re.compile(rb"(?<![0-9])[0-9]{12}(?![0-9])")
FORBIDDEN_NAMES = re.compile(r"(^|/)(\.env[^/]*|id_(rsa|ed25519|ecdsa)[^/]*|[^/]*\.pem|[^/]*\.key|CHANGES-RC1\.md)$")


def extra_patterns() -> list:
    path = os.environ.get("DRUMS_RELEASE_DENYLIST")
    if not path:
        return []
    out = []
    with open(path, encoding="utf-8") as f:
        lines = f.read().splitlines()
    for line in lines:
        line = line.strip()
        if line and not line.startswith("#"):
            out.append(("denylist pattern", re.compile(line.encode())))
    return out


def scan(label: str, data: bytes, patterns, problems: list) -> None:
    for what, rx in patterns:
        if rx.search(data):
            problems.append(f"{label}: {what}")
    for m in TWELVE.finditer(data):
        if hashlib.sha256(m.group(0)).hexdigest() in ACCOUNT_HASHES:
            problems.append(f"{label}: a Drums AWS account id")
            break


def git(*args: str, cwd: str | None = None) -> str:
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True, check=True).stdout


def check_bundle(path: str, name: str, release_commit: str, problems: list, drumsd_root: str, criu_base: str) -> None:
    heads = [ln.split() for ln in git("bundle", "list-heads", path).splitlines() if ln.strip()]
    refs = sorted(r for _, r in heads)
    want = f"refs/heads/release-{name}"
    if refs != [want]:
        problems.append(f"sources/{name}.bundle: heads {refs}, want exactly [{want}]")
        return
    head = heads[0][0]
    with tempfile.TemporaryDirectory() as d:
        git("clone", "-q", "--bare", path, d)
        if name == "drumsd":
            if head != release_commit:
                problems.append(f"sources/drumsd.bundle: head {head} is not RELEASE_COMMIT {release_commit}")
            roots = git("rev-list", "--max-parents=0", head, cwd=d).split()
            if roots != [drumsd_root]:
                problems.append(f"sources/drumsd.bundle: root commit {roots} is not drumsd's ({drumsd_root})")
            rng = [head]
        else:
            if subprocess.run(["git", "merge-base", "--is-ancestor", criu_base, head], cwd=d).returncode != 0:
                problems.append(f"sources/drums-criu.bundle: does not contain CRIU v4.3 ({criu_base[:12]})")
                return
            rng = [f"{criu_base}..{head}"]
        hist = subprocess.run(["git", "log", "-p", "--no-color", "--format=%H %an%n%B", *rng], cwd=d,
                              capture_output=True, check=True).stdout
        scan(f"sources/{name}.bundle history", hist, SECRET_PATTERNS, problems)


def check(tb: str, drumsd_root: str = DRUMSD_ROOT, criu_base: str = CRIU_BASE) -> list:
    problems: list = []
    patterns = FILE_PATTERNS + extra_patterns()
    with tarfile.open(tb, "r:gz") as t:
        members = t.getmembers()
        tops = {m.name.split("/", 1)[0] for m in members}
        if len(tops) != 1 or not re.fullmatch(r"drums-release-[0-9a-f]{12}", next(iter(tops))):
            return [f"want one top directory drums-release-<12 hex>, got {sorted(tops)}"]
        top = next(iter(tops))
        files = {}
        for m in members:
            rel = m.name[len(top) + 1:]
            if m.name.startswith("/") or ".." in m.name.split("/"):
                problems.append(f"{m.name}: absolute or .. path")
            if not (m.isfile() or m.isdir()):
                problems.append(f"{m.name}: not a regular file or directory")
                continue
            if not rel:
                continue
            first = rel.split("/", 1)[0]
            if first not in TOP_ALLOWED:
                problems.append(f"{rel}: not an allowed top-level entry")
            if FORBIDDEN_NAMES.search(rel):
                problems.append(f"{rel}: forbidden file name")
            if rel.endswith(".bundle") and rel.rsplit("/", 1)[0] != "sources":
                problems.append(f"{rel}: a git bundle outside sources/")
            if m.isfile():
                files[rel] = t.extractfile(m).read()
        present = {r.split("/", 1)[0] for r in files} | {m.name[len(top) + 1:].split("/", 1)[0] for m in members if m.isdir()}
        for req in sorted(TOP_REQUIRED - present):
            problems.append(f"{req}: missing")
        srcs = {r.split("/", 1)[1] for r in files if r.startswith("sources/")}
        if srcs != BUNDLES:
            problems.append(f"sources/: holds {sorted(srcs)}, want exactly {sorted(BUNDLES)}")
        wheels = [r for r in files if r.startswith("sdk/")]
        if len(wheels) != 1 or not re.fullmatch(r"sdk/drums[A-Za-z0-9_]*-[0-9][^/-]*-py3-none-any\.whl", wheels[0]):
            problems.append(f"sdk/: holds {sorted(wheels)}, want one drums*-py3-none-any.whl")
        sums = files.get("SHA256SUMS", b"").decode(errors="replace").splitlines()
        listed = {}
        for ln in sums:
            if ln.strip():
                h, _, p = ln.partition("  ")
                listed[p[2:] if p.startswith("./") else p] = h
        for rel, data in files.items():
            if rel == "SHA256SUMS":
                continue
            if listed.get(rel) != hashlib.sha256(data).hexdigest():
                problems.append(f"{rel}: missing from or different in SHA256SUMS")
        for rel, data in sorted(files.items()):
            if rel.startswith("sources/"):
                continue
            scan(rel, data, patterns, problems)
            if rel.endswith(".whl"):
                with zipfile.ZipFile(io.BytesIO(data)) as z:
                    for n in z.namelist():
                        scan(f"{rel}!{n}", z.read(n), patterns, problems)
        release_commit = files.get("RELEASE_COMMIT", b"").decode().strip()
        with tempfile.TemporaryDirectory() as d:
            for b in sorted(BUNDLES & srcs):
                p = os.path.join(d, b)
                with open(p, "wb") as f:
                    f.write(files[f"sources/{b}"])
                check_bundle(p, b[:-len(".bundle")], release_commit, problems, drumsd_root, criu_base)
    return problems


def main(argv: list) -> int:
    if len(argv) != 2:
        print("usage: check-release.py drums-release-<commit12>.tar.gz", file=sys.stderr)
        return 2
    problems = check(argv[1])
    for p in problems:
        print(f"[check-release] REFUSED: {p}", file=sys.stderr)
    if problems:
        return 1
    print(f"[check-release] ok: {os.path.basename(argv[1])}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
