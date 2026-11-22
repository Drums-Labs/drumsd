"""Build the SDK wheel (<dist>-<version>-py3-none-any.whl, e.g. drums_sdk-0.4.0-py3-none-any.whl) from a
Drums-Labs/drums-python-sdk source tree with the standard library only.

    python3 install/build_wheel.py SDK_DIR OUT_DIR        -> prints the wheel's path

SDK_DIR holds pyproject.toml and the package (make-release.sh extracts the commit pinned in lock.json there).

The SDK is pure Python with no dependencies, so the wheel is the package files plus METADATA, WHEEL, RECORD,
entry_points.txt and the licence files (PEP 427, PEP 639). What it reads from pyproject.toml:

    [project] name, version, description, requires-python, dependencies, license (an SPDX string or {text = ...}),
              license-files, urls, scripts
    [tool.setuptools] packages              (default: [project] name)
    [tool.setuptools.package-data]          (non-Python files shipped inside a package, e.g. drums/mcp_tools.json)

Every entry gets a fixed timestamp and mode, so the same sources give the same bytes (the release's SHA256SUMS covers
it). A user without root installs it with `pip install /opt/drums/sdk/drums*.whl` (into a venv or with --user):
nothing is built in the root-owned source tree, which is what failed before (`pip install
/opt/drums/src/drumsd/python` as a non-root user).
"""
from __future__ import annotations

import base64
import fnmatch
import glob
import hashlib
import os
import re
import sys
import zipfile

try:
    import tomllib  # Python 3.11+
except ModuleNotFoundError:  # pragma: no cover - the installer's hosts run 3.12
    tomllib = None

EPOCH = (2026, 1, 1, 0, 0, 0)


def _flat_project(text: str) -> dict:
    """Fallback for Python < 3.11: the flat string keys of [project] only."""
    sect = text.split("[project]", 1)[1].split("\n[", 1)[0]
    out = {}
    for k in ("name", "version", "description", "requires-python", "license"):
        m = re.search(rf'^{k}\s*=\s*"([^"]*)"', sect, re.M)
        if m:
            out[k] = m.group(1)
    m = re.search(r'^license\s*=\s*\{\s*text\s*=\s*"([^"]*)"', sect, re.M)
    if m:
        out["license"] = {"text": m.group(1)}
    return {"project": out}


def load(src: str) -> dict:
    raw = open(os.path.join(src, "pyproject.toml"), "rb").read()
    return tomllib.loads(raw.decode()) if tomllib else _flat_project(raw.decode())


def project(src: str) -> dict:
    """The fields the wheel needs, from src/pyproject.toml."""
    doc = load(src)
    p = doc.get("project", {})
    for k in ("name", "version", "description", "requires-python"):
        if not isinstance(p.get(k), str):
            raise SystemExit(f"pyproject.toml: [project] {k} missing")
    st = doc.get("tool", {}).get("setuptools", {})
    lic = p.get("license")
    return {
        "name": p["name"],
        "version": p["version"],
        "description": p["description"],
        "requires-python": p["requires-python"],
        "dependencies": list(p.get("dependencies", [])),
        # PEP 639: a string is an SPDX expression; the legacy table form is {text = "..."}
        "license_expression": lic if isinstance(lic, str) else None,
        "license": lic.get("text") if isinstance(lic, dict) else None,
        "license_files": list(p.get("license-files", [])),
        "urls": dict(p.get("urls", {})),
        "scripts": dict(p.get("scripts", {})),
        "packages": list(st.get("packages", [p["name"]])),
        "package_data": {k: list(v) for k, v in st.get("package-data", {}).items()},
    }


def dist_name(name: str) -> str:
    """The wheel's file and dist-info name (PEP 427 / PEP 503 normalisation): drums-sdk -> drums_sdk."""
    return re.sub(r"[-_.]+", "_", name).lower()


def package_files(src: str, pkg: str, data: list[str]) -> list[tuple[str, bytes]]:
    base = os.path.join(src, *pkg.split("."))
    if not os.path.isdir(base):
        raise SystemExit(f"package {pkg} not found at {base}")
    files = []
    for root, dirs, fs in os.walk(base):
        dirs[:] = sorted(d for d in dirs if d != "__pycache__")
        for f in sorted(fs):
            full = os.path.join(root, f)
            rel_pkg = os.path.relpath(full, base).replace(os.sep, "/")
            if f.endswith(".py") or any(fnmatch.fnmatch(rel_pkg, pat) for pat in data):
                files.append((os.path.relpath(full, src).replace(os.sep, "/"), open(full, "rb").read()))
    for pat in data:  # a package-data pattern that matches nothing is a packaging bug, not a silent omission
        if not glob.glob(os.path.join(base, pat)):
            raise SystemExit(f"package-data {pkg}: {pat} matches no file")
    return files


def build(src: str, out_dir: str) -> str:
    p = project(src)
    name, ver = p["name"], p["version"]
    top = p["packages"][0]
    init = open(os.path.join(src, *top.split("."), "__init__.py")).read()
    m = re.search(r'^__version__ = "([^"]+)"', init, re.M)
    if not m or m.group(1) != ver:
        raise SystemExit(f"version mismatch: pyproject {ver}, {top}/__init__.py {m.group(1) if m else None}")
    files = []
    for pkg in p["packages"]:
        files += package_files(src, pkg, p["package_data"].get(pkg, []))
    dist = dist_name(name)
    di = f"{dist}-{ver}.dist-info"
    meta = [f"Metadata-Version: {'2.4' if p['license_expression'] or p['license_files'] else '2.1'}",
            f"Name: {name}", f"Version: {ver}", f"Summary: {p['description']}"]
    if p["license_expression"]:
        meta.append(f"License-Expression: {p['license_expression']}")
    elif p["license"]:
        meta.append(f"License: {p['license']}")
    lic_files = []
    for pat in p["license_files"]:
        hits = sorted(glob.glob(os.path.join(src, pat)))
        if not hits:
            raise SystemExit(f"license-files: {pat} matches no file")
        for h in hits:
            rel = os.path.relpath(h, src).replace(os.sep, "/")
            meta.append(f"License-File: {rel}")
            lic_files.append((f"{di}/licenses/{rel}", open(h, "rb").read()))
    meta += [f"Project-URL: {k}, {v}" for k, v in p["urls"].items()]
    meta.append(f"Requires-Python: {p['requires-python']}")
    meta += [f"Requires-Dist: {d}" for d in p["dependencies"]]
    files += lic_files
    files.append((f"{di}/METADATA", ("\n".join(meta) + "\n").encode()))
    files.append((f"{di}/WHEEL", b"Wheel-Version: 1.0\nGenerator: drums build_wheel.py\nRoot-Is-Purelib: true\nTag: py3-none-any\n"))
    if p["scripts"]:
        ep = "[console_scripts]\n" + "".join(f"{k} = {v}\n" for k, v in sorted(p["scripts"].items()))
        files.append((f"{di}/entry_points.txt", ep.encode()))
    files.append((f"{di}/top_level.txt", "".join(f"{pkg.split('.')[0]}\n" for pkg in p["packages"]).encode()))
    rec = []
    for n, b in files:
        h = base64.urlsafe_b64encode(hashlib.sha256(b).digest()).rstrip(b"=").decode()
        rec.append(f"{n},sha256={h},{len(b)}")
    rec.append(f"{di}/RECORD,,")
    files.append((f"{di}/RECORD", ("\n".join(rec) + "\n").encode()))
    os.makedirs(out_dir, exist_ok=True)
    whl = os.path.join(out_dir, f"{dist}-{ver}-py3-none-any.whl")
    with zipfile.ZipFile(whl + ".part", "w", zipfile.ZIP_DEFLATED) as z:
        for n, b in files:
            zi = zipfile.ZipInfo(n, EPOCH)
            zi.external_attr = 0o644 << 16
            zi.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(zi, b)
    os.replace(whl + ".part", whl)
    return whl


if __name__ == "__main__":
    if len(sys.argv) != 3:
        raise SystemExit("usage: build_wheel.py SDK_DIR OUT_DIR")
    print(build(sys.argv[1], sys.argv[2]))
