"""Adapter contract shared by every arm (baseline VM, Drums, anything else)."""

from __future__ import annotations

import abc
from dataclasses import dataclass, field

# Infrastructure phases are planned by adapters. Workload phases (ready,
# warmup, reseed, canary_plant, task, evaluate, canary_probe, state_hash) are
# planned by the runner from the user-supplied workload, identically per arm.
INFRA_PHASES = ("provision", "endpoint", "snapshot", "restore", "reset", "cleanup")
WORKLOAD_PHASES = (
    "ready", "warmup", "reseed", "canary_plant", "task", "evaluate",
    "canary_probe", "state_hash",
)


@dataclass
class Command:
    phase: str
    argv: tuple
    purpose: str
    slot: int | None = None
    attempt: int | None = None
    env: dict = field(default_factory=dict)
    timeout_s: float = 0.0  # 0 means the runner default
    allowed_rc: tuple = (0,)

    def to_plan(self) -> dict:
        return {
            "phase": self.phase, "slot": self.slot, "attempt": self.attempt,
            "purpose": self.purpose, "argv": list(self.argv),
            "env": dict(self.env), "timeout_s": self.timeout_s,
            "allowed_rc": list(self.allowed_rc),
        }


@dataclass
class ExecResult:
    rc: int | None
    stdout: str
    stderr: str
    started: float       # monotonic seconds
    ended: float         # monotonic seconds
    wall_started: str    # ISO-8601 UTC
    timed_out: bool = False
    error: str | None = None  # e.g. "tool_absent: ..."


@dataclass
class Problem:
    code: str
    message: str
    blocking: bool = True  # blocks --execute; always only a warning for dry runs


class Adapter(abc.ABC):
    """One arm. Methods only *plan* commands; the runner executes them."""

    name: str = "adapter"
    is_fake: bool = False

    @abc.abstractmethod
    def preflight(self, workload, concurrency: int) -> list:
        """Return Problem items. Missing tools/backends must be reported here."""

    @abc.abstractmethod
    def describe(self) -> dict:
        """Preregistered configuration and its hash, as recorded in receipts."""

    @abc.abstractmethod
    def capture_environment(self) -> list:
        """Commands whose output records actual versions/resources."""

    @abc.abstractmethod
    def slot_env(self, slot: int) -> dict:
        """Endpoint env (BENCH_*) the workload commands receive for a slot."""

    @abc.abstractmethod
    def provision(self, workload, slot: int) -> list: ...

    @abc.abstractmethod
    def snapshot(self, slot: int) -> list: ...

    @abc.abstractmethod
    def restore(self, slot: int) -> list: ...

    @abc.abstractmethod
    def reset(self, slot: int) -> list: ...

    @abc.abstractmethod
    def cleanup(self) -> list: ...

    def endpoint(self, slot: int) -> list:
        return []

    def verify_capture(self, results: list) -> list:
        """Check captured environment against the preregistration; Problems."""
        return []

    def registry(self) -> dict:
        """Host resources the sweeper may reclaim: {"vms": [{overlay, pidfile, argv0}]}."""
        return {"vms": []}
