"""Sameness gate: are the two arms running the same image, server, checker, task and resources?

`compare` blocks (returns blocking items) when any identity fact differs OR is unknown. Inherent differences
(kernel, display stack, resource enforcement, ...) are not blockers but must be carried, verbatim, into any
report that compares the arms: they are the fidelity confounds.
"""

from __future__ import annotations

import hashlib

from osworld_smoke import pins

from . import FIDELITY_DIFFERENCES, PINNED_SERVER_MAIN_SHA256

IDENTITY_KEYS = ("image_sha256", "task_sha256", "evaluator_command_sha256", "guest_server_sha256", "guest_vcpus",
                 "guest_memory_mib", "workload_hash")


def evaluator_sha() -> str:
    command, shell, expected = pins.evaluator(pins.load_task())
    return hashlib.sha256(f"{shell}|{command}|{expected}".encode()).hexdigest()


def _server(inspection):
    return (inspection or {}).get("server") or {}


def _common(workload, inspection, image_sha256) -> dict:
    srv = _server(inspection)
    return {"image_sha256": image_sha256, "task_sha256": pins.TASK_SHA256, "evaluator_command_sha256": evaluator_sha(),
            "guest_server_sha256": srv.get("sha256"), "guest_server_path": srv.get("path"),
            "guest_server_matches_pinned_public_source": srv.get("matches_pinned_public_source"),
            "workload_hash": getattr(workload, "workload_hash", None)}


def facts_baseline(workload, baseline_cfg: dict, inspection: dict, image_sha256: str) -> dict:
    """`inspection` must be the baseline side's OWN read-only inspection of the image it boots (its actual server
    file and hash), and image_sha256 the hash its run verified. Neither is copied from the other arm."""
    return {**_common(workload, inspection, image_sha256), "guest_vcpus": baseline_cfg.get("vcpus"),
            "guest_memory_mib": baseline_cfg.get("memory_mib"), "arm": "baseline-qemu-warm-snapshot"}


def facts_rootfs(workload, arm_cfg: dict, inspection: dict, image_sha256=None, display_stage=None) -> dict:
    """The image hash defaults to the one the inspection recorded from `image attach` (unknown -> blocks)."""
    sha = image_sha256 if image_sha256 is not None else (inspection or {}).get("image_sha256")
    return {**_common(workload, inspection, sha), "guest_vcpus": arm_cfg.get("vcpus"),
            "guest_memory_mib": arm_cfg.get("memory_mib"), "arm": "sharedos-rootfs",
            "display": {"driver": "xorg-dummy (staged into the slot upper layer)" if display_stage else
                        ((inspection or {}).get("display") or {}).get("decision"),
                        "stage_record": display_stage}}


def compare(a: dict, b: dict) -> dict:
    blocking, matches = [], []
    for k in IDENTITY_KEYS:
        va, vb = a.get(k), b.get(k)
        if va is None or vb is None:
            blocking.append({"fact": k, "why": "unknown on " + ("both" if va is None and vb is None else
                                                                 a.get("arm") if va is None else b.get("arm"))})
        elif va != vb:
            blocking.append({"fact": k, "why": "differs", a.get("arm", "a"): va, b.get("arm", "b"): vb})
        else:
            matches.append(k)
    pa, pb = a.get("guest_server_matches_pinned_public_source"), b.get("guest_server_matches_pinned_public_source")
    both_pinned = pa is True and pb is True
    evidence = {"guest_server_sha256": {a.get("arm", "a"): a.get("guest_server_sha256"), b.get("arm", "b"): b.get("guest_server_sha256")},
                "guest_server_path": {a.get("arm", "a"): a.get("guest_server_path"), b.get("arm", "b"): b.get("guest_server_path")},
                "pinned_public_server_sha256": PINNED_SERVER_MAIN_SHA256,
                "matches_pinned_public_source": {a.get("arm", "a"): pa, b.get("arm", "b"): pb},
                "pinned_source_equal": both_pinned,
                "statement": ("the compared server files are the actual files found in each image; both equal the pinned "
                              "public main.py" if both_pinned else
                              "the compared server files are the actual files found in each image; equality to the pinned "
                              "public main.py is NOT claimed (at least one arm's file does not match it or its match is unknown)")}
    return {"blocking": blocking, "matches": matches, "identical": not blocking, "evidence": evidence,
            "graphics": {"identical_graphics_claimed": False,
                         "baseline": "QEMU virtual GPU, the image's real Xorg driver",
                         "rootfs": (b.get("display") or a.get("display") or {}).get("driver"),
                         "display_stage_record": (b.get("display") or a.get("display") or {}).get("stage_record")},
            "declared_differences": list(FIDELITY_DIFFERENCES),
            "note": "an identical result lifts the identity blockers only; declared_differences must accompany any "
                    "comparison, and the real gates must also have passed"}
