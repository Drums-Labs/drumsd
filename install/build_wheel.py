"""Build the SDK wheel (drums-<version>-py3-none-any.whl) from python/ with the standard library only.

    python3 install/build_wheel.py OUT_DIR        -> prints the wheel's path

The SDK is pure Python with no dependencies, so the wheel is the package files plus METADATA, WHEEL and RECORD
(PEP 427). Every entry gets a fixed timestamp and mode, so the same sources give the same bytes (the release's
SHA256SUMS covers it). A user without root installs it with `pip install /opt/drums/sdk/drums-*.whl` (into a venv or
with --user): nothing is built in the root-owned source tree, which is what failed before (`pip install
/opt/drums/src/drumsd/python` as a non-root user).
"""
from __future__ import annotations

import base64
import hashlib
import os
import re
import sys
import zipfile

HERE = os.path.dirname(os.path.abspath(__file__))
PY = os.path.join(os.path.dirname(HERE), "python")
EPOCH = (2026, 1, 1, 0, 0, 0)


def project():
    """name, version, description, requires-python, license from python/pyproject.toml (flat keys of [project])."""
    text = open(os.path.join(PY, "pyproject.toml")).read()
    sect = text.split("[project]", 1)[1].split("\n[", 1)[0]
    out = {}
    for k in ("name", "version", "description", "requires-python"):
        m = re.search(rf'^{k}\s*=\s*"([^"]*)"', sect, re.M)
        if not m:
            raise SystemExit(f"pyproject.toml: [project] {k} missing")
        out[k] = m.group(1)
    m = re.search(r'^license\s*=\s*\{\s*text\s*=\s*"([^"]*)"', sect, re.M)
    out["license"] = m.group(1) if m else None
    return out


def build(out_dir: str) -> str:
    p = project()
    name, ver = p["name"], p["version"]
    init = open(os.path.join(PY, name, "__init__.py")).read()
    m = re.search(r'^__version__ = "([^"]+)"', init, re.M)
    if not m or m.group(1) != ver:
        raise SystemExit(f"version mismatch: pyproject {ver}, {name}/__init__.py {m.group(1) if m else None}")
    files = []
    for root, dirs, fs in os.walk(os.path.join(PY, name)):
        dirs[:] = sorted(d for d in dirs if d != "__pycache__")
        for f in sorted(fs):
            if f.endswith(".py"):
                full = os.path.join(root, f)
                files.append((os.path.relpath(full, PY).replace(os.sep, "/"), open(full, "rb").read()))
    di = f"{name}-{ver}.dist-info"
    meta = [f"Metadata-Version: 2.1", f"Name: {name}", f"Version: {ver}", f"Summary: {p['description']}",
            f"Requires-Python: {p['requires-python']}"] + ([f"License: {p['license']}"] if p["license"] else [])
    files.append((f"{di}/METADATA", ("\n".join(meta) + "\n").encode()))
    files.append((f"{di}/WHEEL", b"Wheel-Version: 1.0\nGenerator: drums build_wheel.py\nRoot-Is-Purelib: true\nTag: py3-none-any\n"))
    files.append((f"{di}/top_level.txt", f"{name}\n".encode()))
    rec = []
    for n, b in files:
        h = base64.urlsafe_b64encode(hashlib.sha256(b).digest()).rstrip(b"=").decode()
        rec.append(f"{n},sha256={h},{len(b)}")
    rec.append(f"{di}/RECORD,,")
    files.append((f"{di}/RECORD", ("\n".join(rec) + "\n").encode()))
    os.makedirs(out_dir, exist_ok=True)
    whl = os.path.join(out_dir, f"{name}-{ver}-py3-none-any.whl")
    with zipfile.ZipFile(whl + ".part", "w", zipfile.ZIP_DEFLATED) as z:
        for n, b in files:
            zi = zipfile.ZipInfo(n, EPOCH)
            zi.external_attr = 0o644 << 16
            zi.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(zi, b)
    os.replace(whl + ".part", whl)
    return whl


if __name__ == "__main__":
    print(build(sys.argv[1] if len(sys.argv) > 1 else "."))
