"""Stage ONE pinned public Ubuntu jammy package (the Xorg `dummy` video driver) into a slot's UPPER layer.

Why: the pinned OSWorld image has Xorg and GNOME but no Xvfb and no dummy driver, so its own Xorg cannot run headless.
Nothing is written to the image (the lower layer is only read). The added file lives in the slot's upper directory,
is recorded with hashes in `display-stage.json`, and the arm's effective inspection says so explicitly; the original
image inspection is never edited. No host binary is copied: the file comes from the pinned .deb, which is
re-verified here (sha256, size, control fields), its dependencies/ABI are checked against the IMAGE's own dpkg
database, and only one allow-listed path is extracted. Anything unproven fails closed (StageError).

The pin was resolved from the public archive by the author: InRelease (jammy) signature checked with
ubuntu-archive-keyring -> main/binary-amd64/Packages.gz sha256 -> the package's SHA256 -> the .deb. Only the .deb
hash, size and control are re-verified at stage time (offline); the archive chain is recorded in PIN as provenance.

This changes the graphics path relative to the baseline (virtual GPU + real Xorg driver vs. Xorg dummy). That is a
declared difference; identical graphics are never claimed.
"""

from __future__ import annotations

import hashlib
import io
import json
import os
import re
import subprocess
import tarfile

PIN = {
    "package": "xserver-xorg-video-dummy",
    "version": "1:0.3.8-2build1",
    "architecture": "amd64",
    "deb_filename": "xserver-xorg-video-dummy_0.3.8-2build1_amd64.deb",
    "deb_url": "http://archive.ubuntu.com/ubuntu/pool/main/x/xserver-xorg-video-dummy/"
               "xserver-xorg-video-dummy_0.3.8-2build1_amd64.deb",
    "deb_sha256": "2ef60bd404142f7bb3d77a2886ac89d8a0dabc101d87ce7496ebe45f19ae8fc3",
    "deb_size": 9578,
    "depends": "libc6 (>= 2.4), xorg-video-abi-25, xserver-xorg-core (>= 2:21.1.1)",
    "driver_path": "usr/lib/xorg/modules/drivers/dummy_drv.so",
    "driver_sha256": "c4ac3583363b3b3e9c25b1176bd473f32ff64910f7a6ce2897130a761d5142cf",
    "also_in_package_not_staged_prefixes": ["usr/share/doc/", "usr/share/bug/"],
    "archive_provenance": {
        "suite": "jammy", "component": "main", "index": "main/binary-amd64/Packages.gz",
        "index_sha256": "712ee19b50fa5a5963b82b8dd00438f59ef1f088db8e3e042f4306d2b7c89c69",
        "in_release_sha256": "c14060cd8c6d625874dfcb9523a35a395bf4865c28b6b6a82569ef326fe92dc6",
        "in_release_date": "Thu, 21 Apr 2022 17:16:08 UTC",
        "in_release_signature": "Good signature, Ubuntu Archive Automatic Signing Key (2018), checked by the author",
    },
}

RECORD_NAME = "display-stage.json"


class StageError(Exception):
    pass


# ----------------------------------------------------------------------------------------- dpkg data
def parse_dpkg_status(text: str) -> dict:
    """{package: {version, installed, provides}} from /var/lib/dpkg/status (installed packages only)."""
    out = {}
    for para in re.split(r"\n\s*\n", text):
        f = {}
        for line in para.splitlines():
            if line[:1] in (" ", "\t") or ":" not in line:
                continue
            k, v = line.split(":", 1)
            f[k.strip()] = v.strip()
        if "Package" not in f:
            continue
        installed = f.get("Status", "").split()[-1:] == ["installed"] and f.get("Status", "").startswith("install ok")
        provides = [re.split(r"[ (]", x.strip())[0] for x in f.get("Provides", "").split(",") if x.strip()]
        out[f["Package"]] = {"version": f.get("Version"), "installed": installed, "provides": provides}
    return out


def _order(c: str) -> int:
    if c == "~":
        return -1
    if c.isdigit():
        return 0
    if c.isalpha():
        return ord(c)
    return ord(c) + 256


def _cmp_part(a: str, b: str) -> int:
    while a or b:
        da = re.match(r"\D*", a).group()
        db = re.match(r"\D*", b).group()
        for i in range(max(len(da), len(db))):
            ca = _order(da[i]) if i < len(da) else 0
            cb = _order(db[i]) if i < len(db) else 0
            if ca != cb:
                return -1 if ca < cb else 1
        a, b = a[len(da):], b[len(db):]
        na, nb = re.match(r"\d*", a).group(), re.match(r"\d*", b).group()
        ia, ib = int(na or 0), int(nb or 0)
        if ia != ib:
            return -1 if ia < ib else 1
        a, b = a[len(na):], b[len(nb):]
    return 0


def _split_version(v: str):
    epoch, rest = (v.split(":", 1) if ":" in v else ("0", v))
    up, rev = (rest.rsplit("-", 1) if "-" in rest else (rest, ""))
    return int(epoch), up, rev


def version_compare(a: str, b: str) -> int:
    """Debian version ordering (policy 5.6.12): -1, 0, 1."""
    ea, ua, ra = _split_version(a)
    eb, ub, rb = _split_version(b)
    if ea != eb:
        return -1 if ea < eb else 1
    return _cmp_part(ua, ub) or _cmp_part(ra, rb)


_OPS = {"<<": lambda c: c < 0, "<=": lambda c: c <= 0, "=": lambda c: c == 0, ">=": lambda c: c >= 0, ">>": lambda c: c > 0}


def check_depends(depends: str, status: dict) -> list:
    """Problems (empty = every dependency is satisfied by the IMAGE's installed packages). A versioned dependency
    is satisfied only by a real package of that version; a virtual one (xorg-video-abi-N) only by an installed
    package that Provides it. Unknown or unsatisfied = a problem."""
    problems = []
    for clause in [c.strip() for c in depends.split(",") if c.strip()]:
        ok = False
        for alt in [a.strip() for a in clause.split("|")]:
            m = re.fullmatch(r"([a-z0-9][a-z0-9+.\-]*)(?::\w+)?\s*(?:\((<<|<=|=|>=|>>)\s*([^)\s]+)\))?", alt)
            if not m:
                problems.append(f"cannot parse dependency {alt!r}")
                ok = True            # already a problem; do not double-report
                break
            name, op, want = m.groups()
            real = status.get(name)
            if real and real["installed"] and (op is None or _OPS[op](version_compare(real["version"], want))):
                ok = True
                break
            if op is None and any(p["installed"] and name in p["provides"] for p in status.values()):
                ok = True
                break
        if not ok:
            problems.append(f"dependency not satisfied by the image: {clause}")
    return problems


# ----------------------------------------------------------------------------------------- the deb
def _run(argv):
    r = subprocess.run(argv, capture_output=True)
    return r.returncode, r.stdout, r.stderr


def _sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def verify_deb(deb: str, pin: dict, run=None) -> dict:
    """sha256, size and control fields of the .deb against the pin; returns the control fields."""
    run = run or _run
    try:
        size = os.path.getsize(deb)
    except OSError as e:
        raise StageError(f"cannot read the deb: {e}")
    if size != pin["deb_size"]:
        raise StageError(f"deb size {size} != pinned {pin['deb_size']}")
    got = _sha256(deb)
    if got != pin["deb_sha256"]:
        raise StageError(f"deb sha256 {got} != pinned {pin['deb_sha256']}")
    control = {}
    for field in ("Package", "Version", "Architecture", "Depends"):
        rc, out, err = run(["dpkg-deb", "-f", deb, field])
        if rc != 0:
            raise StageError(f"dpkg-deb -f {field} failed: {err.decode(errors='replace')[:200]}")
        control[field] = out.decode().strip()
    for field, key in (("Package", "package"), ("Version", "version"), ("Architecture", "architecture"), ("Depends", "depends")):
        if control[field] != pin[key]:
            raise StageError(f"deb control {field} {control[field]!r} != pinned {pin[key]!r}")
    return control


def read_driver(deb: str, pin: dict, run=None) -> bytes:
    """The single allow-listed file from the deb's filesystem tar. Any other member must be a known ignorable one
    (docs, bug script); anything else means the package is not what was pinned and fails closed."""
    run = run or _run
    rc, out, err = run(["dpkg-deb", "--fsys-tarfile", deb])
    if rc != 0:
        raise StageError(f"dpkg-deb --fsys-tarfile failed: {err.decode(errors='replace')[:200]}")
    want = "./" + pin["driver_path"]
    data = None
    with tarfile.open(fileobj=io.BytesIO(out), mode="r:") as tf:
        for m in tf.getmembers():
            name = m.name
            if name == want:
                if not m.isreg():
                    raise StageError(f"{name} is not a regular file")
                if m.mode & 0o7000:
                    raise StageError(f"{name} has setuid/setgid/sticky bits")
                data = tf.extractfile(m).read()
                continue
            rel = name[2:] if name.startswith("./") else name
            if (m.isdir() and (pin["driver_path"].rsplit("/", 1)[0] + "/").startswith(rel.rstrip("/") + "/")) or rel in ("", "."):
                continue          # the directories leading to the driver
            ign = pin["also_in_package_not_staged_prefixes"]
            if any((rel + "/").startswith(p) for p in ign):
                continue                                        # a doc/bug entry (file, dir or link) under an ignorable prefix
            if m.isdir() and any(p.startswith(rel.rstrip("/") + "/") for p in ign):
                continue                                        # the directories leading to those prefixes
            raise StageError(f"unexpected content in the deb, nothing staged: {name}")
    if data is None:
        raise StageError(f"{pin['driver_path']} not found in the deb")
    if hashlib.sha256(data).hexdigest() != pin["driver_sha256"]:
        raise StageError("the driver inside the deb does not match the pinned driver sha256")
    return data


# ----------------------------------------------------------------------------------------- staging
def stage(lower: str, deb: str, upper: str, slot_dir: str, pin: dict = None, run=None) -> dict:
    """Verify, then write the driver into `upper` and the provenance record into `slot_dir`. Returns the record.
    Refuses if the image already has the file (the inspection should then have seen it), if the image's dpkg
    database is unreadable, or if any dependency / the Xorg video ABI is not satisfied by the image."""
    pin = pin or PIN
    control = verify_deb(deb, pin, run)
    try:
        status_text = open(os.path.join(lower, "var/lib/dpkg/status"), errors="replace").read()
    except OSError as e:
        raise StageError(f"cannot read the image's dpkg status (dependency/ABI unproven): {e}")
    status = parse_dpkg_status(status_text)
    core = status.get("xserver-xorg-core")
    if not core or not core["installed"]:
        raise StageError("xserver-xorg-core is not installed in the image: no Xorg ABI to match")
    problems = check_depends(control["Depends"], status)
    if problems:
        raise StageError("; ".join(problems))
    if os.path.lexists(os.path.join(lower, pin["driver_path"])):
        raise StageError(f"the image already contains {pin['driver_path']}: nothing to stage")
    data = read_driver(deb, pin, run)
    dest = os.path.join(upper, pin["driver_path"])
    if os.path.lexists(dest):
        raise StageError(f"{dest} already exists in the slot's upper layer")
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    with open(dest, "xb") as f:
        f.write(data)
    os.chmod(dest, 0o644)
    record = {"staged": True, "package": pin["package"], "version": pin["version"], "architecture": pin["architecture"],
              "deb_sha256": pin["deb_sha256"], "deb_url": pin["deb_url"], "depends": control["Depends"],
              "image_xserver_xorg_core_version": core["version"],
              "image_xorg_video_abi_provided_by": "xserver-xorg-core",
              "files_added_to_upper": [{"path": "/" + pin["driver_path"], "sha256": pin["driver_sha256"], "bytes": len(data)}],
              "files_in_deb_not_staged": pin["also_in_package_not_staged_prefixes"],
              "archive_provenance": pin["archive_provenance"],
              "image_modified": False, "host_binaries_copied": False,
              "note": "added to the slot's upper layer only; the original image inspection is unchanged"}
    with open(os.path.join(slot_dir, RECORD_NAME), "w") as f:
        json.dump(record, f, indent=2)
    return record


def inspect_staged(inspection: dict, record: dict) -> dict:
    """The effective inspection for a slot that has the driver staged: the display decision becomes
    xorg_dummy_staged and the original is kept verbatim under `image_display`."""
    if not record or not record.get("staged"):
        return inspection
    if inspection["display"]["decision"] != "needs_added_display_package":
        raise StageError("a driver was staged but the image's own display decision is not needs_added_display_package")
    if "Xorg" not in inspection["binaries"]:
        raise StageError("the image has no Xorg binary to run with the staged driver")
    eff = json.loads(json.dumps(inspection))
    eff["image_display"] = inspection["display"]
    eff["display"] = {"decision": "xorg_dummy_staged", "xorg_dummy_driver": True, "added_by_stage": record,
                      "added_packages_needed": [],
                      "note": "driver added to the slot's upper layer from a pinned deb; the image itself lacks it"}
    return eff


def load_record(slot_dir: str):
    try:
        with open(os.path.join(slot_dir, RECORD_NAME)) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None
