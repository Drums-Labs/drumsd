"""User-supplied workload manifest and money inputs. Everything fails closed."""

from __future__ import annotations

import hashlib
import json
import math
import os
from dataclasses import dataclass


class ConfigError(Exception):
    pass


def sha256_file(path: str) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def canonical(obj) -> str:
    return json.dumps(obj, sort_keys=True, separators=(",", ":"))


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode()).hexdigest()


def finite_number(x) -> bool:
    """A real, finite int/float. Rejects bool, NaN, +/-Infinity (json.loads accepts the last three)."""
    return isinstance(x, (int, float)) and not isinstance(x, bool) and math.isfinite(x)


def _int(x) -> bool:
    return isinstance(x, int) and not isinstance(x, bool)


def _need(d: dict, key: str, where: str):
    if not isinstance(d, dict) or key not in d or d[key] in (None, "", []):
        raise ConfigError(f"{where}: missing required field '{key}'")
    return d[key]


def _cmd(d: dict, key: str, where: str, optional=False) -> dict | None:
    if optional and key not in d:
        return None
    c = _need(d, key, where)
    argv = _need(c, "argv", f"{where}.{key}")
    if not (isinstance(argv, list) and all(isinstance(a, str) for a in argv)):
        raise ConfigError(f"{where}.{key}.argv must be a list of strings")
    t = _need(c, "timeout_s", f"{where}.{key}")
    if not finite_number(t) or t <= 0:
        raise ConfigError(f"{where}.{key}.timeout_s must be a finite number > 0")
    return {"argv": argv, "timeout_s": float(t)}


@dataclass
class Workload:
    raw: dict
    base_dir: str
    name: str
    image_path: str
    image_sha256: str
    image_format: str
    vcpus: int
    memory_mib: int
    seeds: list
    min_attempts: int
    min_valid_fraction: float
    success_tolerance: float
    commands: dict
    workload_hash: str
    file_hashes: dict

    @property
    def attempts(self) -> int:
        return len(self.seeds)

    def guest_resources(self) -> dict:
        return {"vcpus": self.vcpus, "memory_mib": self.memory_mib}


def load_workload(path: str) -> Workload:
    try:
        with open(path) as f:
            raw = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        raise ConfigError(f"cannot read workload manifest {path}: {e}")
    base = os.path.dirname(os.path.abspath(path))
    name = _need(raw, "name", "workload")
    img = _need(raw, "image", "workload")
    image_path = _need(img, "path", "workload.image")
    if not os.path.isabs(image_path):
        image_path = os.path.join(base, image_path)
    sha = _need(img, "sha256", "workload.image")
    if len(sha) != 64 or any(c not in "0123456789abcdef" for c in sha):
        raise ConfigError("workload.image.sha256 must be 64 lowercase hex chars")
    fmt = img.get("format", "qcow2")
    if fmt not in ("qcow2", "raw"):
        raise ConfigError("workload.image.format must be qcow2 or raw")
    res = _need(raw, "guest_resources", "workload")
    vcpus, mem = _need(res, "vcpus", "guest_resources"), _need(res, "memory_mib", "guest_resources")
    if not (_int(vcpus) and vcpus > 0 and _int(mem) and mem > 0):
        raise ConfigError("guest_resources vcpus/memory_mib must be positive ints")
    seeds = _need(raw, "seeds", "workload")
    if not (isinstance(seeds, list) and all(_int(s) for s in seeds)
            and len(set(seeds)) == len(seeds)):
        raise ConfigError("workload.seeds must be a list of unique ints")
    min_attempts = _need(raw, "min_attempts", "workload")
    if not _int(min_attempts) or min_attempts < 1 or len(seeds) < min_attempts:
        raise ConfigError("workload.min_attempts must be >=1 and <= len(seeds)")
    mvf = _need(raw, "min_valid_fraction", "workload")
    if not (finite_number(mvf) and 0 < mvf <= 1):
        raise ConfigError("workload.min_valid_fraction must be in (0, 1]")
    tol = _need(raw, "success_tolerance", "workload")
    if not (finite_number(tol) and 0 <= tol <= 1):
        raise ConfigError("workload.success_tolerance must be in [0, 1]")

    commands = {}
    for key in ("ready", "state_hash", "agent_policy", "checker"):
        commands[key] = _cmd(raw, key, "workload")
    canary = _need(raw, "canary", "workload")
    commands["canary_plant"] = _cmd(canary, "plant", "workload.canary")
    commands["canary_probe"] = _cmd(canary, "probe", "workload.canary")
    for key in ("warmup", "reseed"):
        c = _cmd(raw, key, "workload", optional=True)
        if c:
            commands[key] = c

    file_hashes = {}
    for key, c in sorted(commands.items()):
        exe = c["argv"][0]
        p = exe if os.path.isabs(exe) else os.path.join(base, exe)
        file_hashes[key] = sha256_file(p) if os.path.isfile(p) else None
    hashed = {k: v for k, v in raw.items()}
    hashed["image"] = {"sha256": sha, "format": fmt}
    wh = sha256_text(canonical({"manifest": hashed, "files": file_hashes}))
    return Workload(raw, base, name, image_path, sha, fmt, vcpus, mem, seeds,
                    min_attempts, float(mvf), float(tol), commands, wh, file_hashes)


@dataclass
class Money:
    currency: str
    host_hourly_rate: float
    rate_source: str
    billed_hosts: int
    granularity_s: float
    minimum_billed_s: float
    fixed_costs: list
    cost_cap: float
    max_wall_seconds: float

    def billed_seconds(self, window_s: float) -> float:
        s = max(window_s, self.minimum_billed_s)
        g = self.granularity_s
        return -(-s // g) * g

    def host_cost(self, window_s: float) -> float:
        return self.billed_seconds(window_s) * self.billed_hosts / 3600.0 * self.host_hourly_rate

    def fixed_total(self) -> float:
        return sum(c["amount"] for c in self.fixed_costs)


def load_money(path: str | None) -> Money:
    if not path:
        raise ConfigError("money inputs absent: pass --money (no cost can be computed without a declared rate)")
    try:
        with open(path) as f:
            raw = json.load(f)
    except (OSError, json.JSONDecodeError) as e:
        raise ConfigError(f"cannot read money file {path}: {e}")
    rate = _need(raw, "host_hourly_rate", "money")
    if not finite_number(rate) or rate <= 0:
        raise ConfigError("money.host_hourly_rate must be a finite number > 0")
    hosts = _need(raw, "billed_hosts", "money")
    if not _int(hosts) or hosts < 1:
        raise ConfigError("money.billed_hosts must be an int >= 1")
    if "fixed_costs" not in raw or raw.get("fixed_costs_declared_complete") is not True:
        raise ConfigError("money: declare fixed_costs (may be []) and set fixed_costs_declared_complete=true")
    fixed = raw["fixed_costs"]
    for c in fixed:
        _need(c, "name", "money.fixed_costs[]")
        if not finite_number(c.get("amount")) or c["amount"] < 0:
            raise ConfigError("money.fixed_costs[].amount must be a finite number >= 0")
    cap = _need(raw, "cost_cap", "money")
    mws = _need(raw, "max_wall_seconds", "money")
    gran, minb = raw.get("billing_granularity_seconds", 1), raw.get("minimum_billed_seconds", 0)
    if not (finite_number(cap) and cap > 0 and finite_number(mws) and mws > 0):
        raise ConfigError("money.cost_cap and money.max_wall_seconds must be finite numbers > 0")
    if not (finite_number(gran) and gran > 0 and finite_number(minb) and minb >= 0):
        raise ConfigError("money billing granularity must be finite > 0 and minimum billed seconds finite >= 0")
    return Money(_need(raw, "currency", "money"), float(rate), _need(raw, "rate_source", "money"),
                 hosts, float(raw.get("billing_granularity_seconds", 1)),
                 float(raw.get("minimum_billed_seconds", 0)), fixed, float(cap), float(mws))
