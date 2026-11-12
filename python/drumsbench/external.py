"""External arm (e.g. Drums), reached only through a user-supplied command.

The harness contains no code for the system under test. The command is invoked
as `<command...> <phase> --slot N` for provision, endpoint, snapshot, restore,
reset (slot-scoped) and `cleanup` / `describe` / `environment` (global). Exit 0
means success. `endpoint` must print a JSON object of env vars (BENCH_*) that
the workload commands need to reach that slot; `describe` must print the
arm's preregistered configuration as JSON; `environment` prints actual
versions/resources as text. If no command is configured the arm is absent and
every run fails closed.
"""

from __future__ import annotations

import json
import os
import shutil

from .contract import Adapter, Command, Problem
from .workload import sha256_text


class ExternalArm(Adapter):
    def __init__(self, name: str, command: list | None, state_dir: str, preregistered_sha256: str | None = None):
        self.name = name
        self.command = list(command) if command else None
        self.state_dir = os.path.abspath(state_dir)
        self.declared_hash = preregistered_sha256
        self._slots: set = set()

    def _cmd(self, verb: str, slot: int | None, why: str, phase: str | None = None, **kw) -> Command:
        argv = [*(self.command or ["<arm-command-absent>"]), verb] + (["--slot", str(slot)] if slot is not None else [])
        return Command(phase or verb, tuple(argv), why, slot=slot, **kw)

    def preflight(self, workload, concurrency: int) -> list:
        if not self.command:
            return [Problem("arm_absent", f"arm '{self.name}' has no command configured; "
                            "it is an absent external arm and nothing is simulated")]
        exe = self.command[0]
        if shutil.which(exe) is None and not os.path.isfile(exe):
            return [Problem("arm_absent", f"arm command {exe!r} not found")]
        return [] if self.declared_hash else [
            Problem("arm_not_preregistered", "external arm needs preregistered_sha256 of its describe output")]

    def describe(self) -> dict:
        return {"arm": self.name, "command": self.command, "preregistered_sha256": self.declared_hash,
                "note": "configuration is whatever `describe` printed; recorded verbatim in receipts"}

    def capture_environment(self) -> list:
        if not self.command:
            return []
        return [self._cmd("describe", None, "arm describe", phase="capture"),
                self._cmd("environment", None, "arm environment", phase="capture")]

    def verify_capture(self, results: list) -> list:
        """results: [(Command, ExecResult)] from capture_environment()."""
        by = {cmd.argv[-1]: res for cmd, res in results}
        out = []
        for verb in ("describe", "environment"):
            res = by.get(verb)
            if res is None:
                out.append(Problem(f"arm_{verb}_missing", f"no `{verb}` result captured"))
            elif res.rc != 0 or res.error:
                out.append(Problem(f"arm_{verb}_failed",
                                   f"`{verb}` rc={res.rc} error={res.error} stderr={res.stderr[-500:]!r}"))
            elif not res.stdout.strip():
                out.append(Problem(f"arm_{verb}_malformed", f"`{verb}` printed nothing"))
        if out:
            return out
        try:
            json.loads(by["describe"].stdout)
        except json.JSONDecodeError as e:
            return [Problem("arm_describe_malformed", f"`describe` stdout is not JSON: {e}")]
        if describe_hash(by["describe"].stdout) != self.declared_hash:
            return [Problem("arm_not_preregistered",
                            f"sha256 of `describe` output is {describe_hash(by['describe'].stdout)}, "
                            f"preregistered {self.declared_hash}")]
        return []

    def slot_env(self, slot: int) -> dict:
        return {"BENCH_SLOT": slot}  # endpoint phase adds the rest

    def provision(self, workload, slot: int) -> list:
        self._slots.add(slot)
        return [self._cmd("provision", slot, "provision slot")]

    def endpoint(self, slot: int) -> list:
        return [self._cmd("endpoint", slot, "report workload endpoint env as JSON")]

    def snapshot(self, slot: int) -> list:
        return [self._cmd("snapshot", slot, "save clean state")]

    def restore(self, slot: int) -> list:
        return [self._cmd("restore", slot, "restore clean state before attempt")]

    def reset(self, slot: int) -> list:
        return [self._cmd("reset", slot, "reset slot after attempt")]

    def cleanup(self) -> list:
        return [self._cmd("cleanup", None, "teardown everything", timeout_s=300)]

    def register_slots(self, n: int) -> None:
        self._slots.update(range(n))


def describe_hash(printed: str) -> str:
    return sha256_text(printed.strip())
