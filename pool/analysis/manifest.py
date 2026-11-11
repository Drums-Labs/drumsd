#!/usr/bin/env python3
"""Write raw/MANIFEST.json: every pin a stranger needs to re-run T3.

    python3 pool/analysis/manifest.py RAWROOT --repo REPO --ami-json AMIS.json [--release TARBALL]

RAWROOT holds install-receipt.json (from the image build) and one directory per run (hosts.json, phases.jsonl,
pool-config.json, trainer/trainer.json).
"""
import argparse
import glob
import hashlib
import json
import os
import subprocess


def sha(p):
    h = hashlib.sha256()
    with open(p, "rb") as f:
        for b in iter(lambda: f.read(1 << 20), b""):
            h.update(b)
    return h.hexdigest()


def git(repo, *a):
    return subprocess.run(["git", "-C", repo, *a], capture_output=True, text=True).stdout.strip()


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("raw")
    ap.add_argument("--repo", required=True)
    ap.add_argument("--ami-json", required=True)
    ap.add_argument("--release", default=None)
    a = ap.parse_args()
    rec = json.load(open(os.path.join(a.raw, "install-receipt.json")))
    lock = rec.get("lock", {})
    m = {
        "drums_pool": {"repo": "Drums-Labs/drumsd", "branch": git(a.repo, "rev-parse", "--abbrev-ref", "HEAD"), "commit": git(a.repo, "rev-parse", "HEAD"),
                       "cargo_lock_sha256": sha(os.path.join(a.repo, "Cargo.lock"))},
        "runtime_release": {"drumsd_commit": rec.get("drumsd_commit"), "sources": lock.get("sources"), "rust": lock.get("rust_toolchain"),
                            "apt_snapshot": (lock.get("apt") or {}).get("snapshot"), "image": (lock.get("files") or {}).get("osworld_image_qcow2"),
                            "criu_sha256_on_image": rec.get("criu_sha256"), "drumsd_sha256_on_image": rec.get("drumsd_sha256"), "kernel": rec.get("kernel"),
                            "install_seconds": rec.get("total_seconds"), "slot_profile": "hardened (RC1 default)"},
        "amis": json.load(open(a.ami_json)),
        "observability": json.load(open(os.path.join(a.repo, "pool", "deploy", "obs-lock.json"))),
        "sdk": "drums 0.2.5 (the release's sdk/ wheel)",
        "runs": {},
    }
    if a.release:
        m["runtime_release"]["tarball_sha256"] = sha(a.release)
    for d in sorted(glob.glob(os.path.join(a.raw, "*"))):
        ph = os.path.join(d, "phases.jsonl")
        if not os.path.exists(ph):
            continue
        phases = [json.loads(l) for l in open(ph) if l.strip()]
        hosts = json.load(open(os.path.join(d, "hosts.json"))) if os.path.exists(os.path.join(d, "hosts.json")) else []
        tr = json.load(open(os.path.join(d, "trainer", "trainer.json"))) if os.path.exists(os.path.join(d, "trainer", "trainer.json")) else {}
        m["runs"][os.path.basename(d)] = {
            "hosts": [{k: h.get(k) for k in ("name", "id", "type", "region", "az", "lifecycle", "vcpus")} | {"mem_gib": (h.get("firstboot") or {}).get("mem_gib")} for h in hosts],
            "pool_config": json.load(open(os.path.join(d, "pool-config.json"))) if os.path.exists(os.path.join(d, "pool-config.json")) else None,
            "trainer_args": tr.get("args"), "tasks": tr.get("tasks"), "seed": (tr.get("args") or {}).get("seed"),
            "phases": {p["phase"]: p.get("utc") for p in phases},
        }
    json.dump(m, open(os.path.join(a.raw, "MANIFEST.json"), "w"), indent=1, default=str)
    print("wrote", os.path.join(a.raw, "MANIFEST.json"))


if __name__ == "__main__":
    main()
