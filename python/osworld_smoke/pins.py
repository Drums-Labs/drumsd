"""Public pins. Everything here is verified before use; nothing is trusted by name."""

from __future__ import annotations

import hashlib
import json
import os
import urllib.request

OSWORLD_COMMIT = "04718eab9327856efe8192cc58161acc7a7627e2"
TASK_ID = "e0df059f-28a6-4169-924f-b9623e7184cc"
TASK_URL = (f"https://raw.githubusercontent.com/xlang-ai/OSWorld/{OSWORLD_COMMIT}/"
            f"evaluation_examples/examples/os/{TASK_ID}.json")
TASK_SHA256 = "a507ec6c35284c1413efe0d8a325b41a1626522221efc14f153742a865f1c04d"

HF_COMMIT = "a5d9c3eaae98eebf6e3a0beb84e7e47cf72ae133"
IMAGE_ZIP_URL = (f"https://huggingface.co/datasets/xlangai/ubuntu_osworld/resolve/{HF_COMMIT}/Ubuntu.qcow2.zip")
IMAGE_ZIP_SHA256 = "b795b6cd4c69b252c1b4f10150a347795555032501b60fd031751ed09b896712"
IMAGE_ZIP_MAX_BYTES = 12274945039
IMAGE_QCOW_SHA256 = "6bf667a852b3c307f61d9f09c42559351f45e0607e428b4997becf534cf4d313"

BUNDLED_TASK = os.path.join(os.path.dirname(__file__), "data", "task.json")


class PinError(Exception):
    pass


def _verify_bytes(raw: bytes) -> None:
    got = hashlib.sha256(raw).hexdigest()
    if got != TASK_SHA256:
        raise PinError(f"task JSON sha256 {got} != pinned {TASK_SHA256}")


def load_task(path: str | None = None) -> dict:
    """Load the task and re-verify its pinned sha256 (called by every command)."""
    with open(path or BUNDLED_TASK, "rb") as f:
        raw = f.read()
    _verify_bytes(raw)
    task = json.loads(raw)
    ev = task.get("evaluator", {})
    if (task.get("id") != TASK_ID or ev.get("func") != "exact_match"
            or ev.get("result", {}).get("type") != "vm_command_line"
            or ev.get("expected", {}).get("type") != "rule"):
        raise PinError("task structure differs from the shape this smoke was written for")
    return task


def fetch_task(dest: str) -> str:
    """Fresh fetch of the pinned task from GitHub, verified before it is written."""
    with urllib.request.urlopen(TASK_URL, timeout=30) as r:
        raw = r.read()
    _verify_bytes(raw)
    with open(dest, "xb") as f:
        f.write(raw)
    return dest


def evaluator(task: dict) -> tuple:
    """(command, shell, expected_stdout) exactly as OSWorld's getter/metric use them."""
    ev = task["evaluator"]
    return (ev["result"]["command"], bool(ev["result"].get("shell", False)),
            ev["expected"]["rules"]["expected"])


def setup_steps(task: dict, password: str, width: int, height: int) -> list:
    """The task's own `config` steps with OSWorld's placeholder substitution."""
    def sub(s: str) -> str:
        return (s.replace("{CLIENT_PASSWORD}", password).replace("{SCREEN_WIDTH_HALF}", str(width // 2))
                .replace("{SCREEN_HEIGHT_HALF}", str(height // 2))
                .replace("{SCREEN_WIDTH}", str(width)).replace("{SCREEN_HEIGHT}", str(height)))
    out = []
    for step in task["config"]:
        if step["type"] != "execute":
            raise PinError(f"unsupported setup step type {step['type']!r}")
        p = step["parameters"]
        cmd = sub(p["command"]) if isinstance(p["command"], str) else [sub(a) for a in p["command"]]
        out.append({"command": cmd, "shell": bool(p.get("shell", False))})
    return out


if __name__ == "__main__":
    import sys
    if len(sys.argv) == 3 and sys.argv[1] == "fetch":
        print(fetch_task(sys.argv[2]))
    elif len(sys.argv) == 3 and sys.argv[1] == "verify":
        load_task(sys.argv[2])
        print("ok", TASK_SHA256)
    else:
        sys.exit("usage: python -m osworld_smoke.pins fetch DEST | verify PATH")
