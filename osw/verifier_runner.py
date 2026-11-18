"""The sandboxed verifier runner (docs/design/verifier-sandbox.md). drumsd embeds this file and runs it, inside the
verifier sandbox, with the checker venv's `python -I -B verifier_runner.py SPEC.json`. Standard library only.

SPEC: {"verifier": PATH, "function": NAME, "options": {...}, "task": {...}, "workdir": DIR}

The checker's `ctx` reaches the desktop only through drumsd, one JSON line per request on the runner's own stdin and
stdout (the checker's print() goes to stderr):

    -> {"op": "read_file", "path": P}                       <- {"ok": true, "b64": ...} | {"ok": true, "missing": true}
    -> {"op": "run", "command": C, "shell": B, "timeout": T} <- {"ok": true, "result": {"returncode", "output", "error"}}
                                                             <- {"ok": false, "error": "..."} (raised in the checker)
    -> {"op": "verdict", "score": S, "info": I, "error": E, "error_class": K}   the last line, exactly once

error_class: none | exception | bad_return (the rules of drums_osw.lab_score).
"""
import base64
import importlib.util
import json
import os
import sys
import traceback

PROTO_IN = os.fdopen(os.dup(0), "rb")
PROTO_OUT = os.fdopen(os.dup(1), "wb", buffering=0)
_null = os.open(os.devnull, os.O_RDONLY)
os.dup2(_null, 0)
os.dup2(2, 1)                       # the checker's own prints never reach the protocol
sys.stdout = sys.stderr


def _send(msg):
    PROTO_OUT.write(json.dumps(msg, default=str).encode() + b"\n")


def _readline():
    buf = bytearray()
    while True:
        b = PROTO_IN.readline()
        if not b:
            raise RuntimeError("drumsd closed the verifier channel")
        buf += b
        if buf.endswith(b"\n"):
            return json.loads(buf)


def _call(msg):
    _send(msg)
    r = _readline()
    if not r.get("ok"):
        raise RuntimeError(r.get("error") or "request refused")
    return r


class Ctx:
    """What a lab verifier sees of the episode's desktop (the same API as the host-side helper's Ctx)."""

    def __init__(self, task, options, workdir):
        self.task, self.options, self.workdir = task, options, workdir

    def read_file(self, path):
        """The file's bytes, or None if it does not exist. Any other failure raises: a checker error, never a 0."""
        r = _call({"op": "read_file", "path": str(path)})
        return None if r.get("missing") else base64.b64decode(r["b64"])

    def run(self, command, shell=False, timeout=None):
        """Run a command in the desktop as its user; {"returncode", "output", "error"} (no exception on non-zero)."""
        return _call({"op": "run", "command": command, "shell": bool(shell), "timeout": timeout})["result"]


def lab_score(out):
    info = None
    if isinstance(out, dict):
        out, info = out.get("score"), out.get("info")
    if isinstance(out, bool):
        out = float(out)
    if not isinstance(out, (int, float)) or out != out or out in (float("inf"), float("-inf")):
        raise TypeError(f"a verifier must return a finite number or {{'score': number}}, got {out!r}")
    return float(out), info


def main(argv):
    with open(argv[1]) as f:
        spec = json.load(f)
    workdir = spec.get("workdir") or "/tmp/work"
    os.makedirs(workdir, exist_ok=True)
    verdict = {"op": "verdict", "score": None, "info": None, "error": None, "error_class": "none"}
    try:
        mspec = importlib.util.spec_from_file_location("drums_sandboxed_verifier", spec["verifier"])
        mod = importlib.util.module_from_spec(mspec)
        mspec.loader.exec_module(mod)
        name = spec.get("function") or "reward"
        fn = getattr(mod, name, None)
        if not callable(fn):
            raise AttributeError(f"verifier has no function {name!r}")
        options = spec.get("options") or {}
        out = fn(Ctx(spec.get("task"), options, workdir), **options)
        try:
            verdict["score"], verdict["info"] = lab_score(out)
            json.dumps(verdict["info"])
        except TypeError as e:
            verdict.update(score=None, info=None, error=str(e)[:800], error_class="bad_return")
    except BaseException as e:  # noqa: BLE001 - reported, never scored (SystemExit included)
        verdict.update(score=None, info=None, error=f"{type(e).__name__}: {e}"[:800] + "\n" + traceback.format_exc(limit=4)[-1200:],
                       error_class="exception")
    _send(verdict)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
