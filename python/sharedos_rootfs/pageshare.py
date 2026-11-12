"""Pageshare: sibling slots restored from ONE checkpoint share its private memory copy-on-write, and every slot keeps ONE cgroup for
its whole life (no cgroup is destroyed per reset).  Harness-side, on top of branch.py; the pinned sources are untouched.

    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.pageshare pool-init
    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.pageshare serve --from S [--criu PATH]
    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.pageshare branch --slot T --from S [--mode template|pool]
    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.pageshare <restore|reset|endpoint> --slot N
    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.pageshare stop-server [--socket S]
    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.pageshare server-status [--socket S]
    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.pageshare cleanup

Memory: `serve` starts the patched CRIU as a template server (`criu restore --template-server SOCK -D <S images>`): it builds every task's
private anonymous memory of the checkpoint once, in its own address space.  A restore in mode `template` runs the SAME restore command line
as branch.py (private mount namespace, overlay as --root, action-script lock gate, all gates after it) but hands it, with its stdio, mount
namespace and cgroup, to the server, whose forked child performs it; every restored task inherits the template copy-on-write.  The VMAs
stay private anonymous mappings.  Mode `pool` is the ordinary private-copy restore (criu reads the pages) inside the persistent cgroup.

Cgroups (finding 11 of linux-density 20261026T0641Z): instead of one systemd scope per restore (`drumsbench-<m>-r<gen>.scope`, destroyed
at the next reset and left dying while its page cache lives), every slot gets ONE cgroup `drumsbench-<m>-pool.scope` inside a delegated
pool scope (`drumsbench-pool.scope`, Delegate=yes, held by a keeper process), created once with the same limits (cpu.max, memory.max,
memory.swap.max, pids.max) and reused by every restore of that slot.  A reset kills the slot's tree (the pinned scoped sweep, then
cgroup.kill as a backstop), proves the cgroup empty, and restores into it again.

Template servers (tmplsrv.py): each runs in its own protected cgroup at oom_score_adj -900 and is checked before every template
restore; a dead one is restarted from its spec and the event recorded (template-events.jsonl).  Every slot's restore runs at
oom_score_adj +300, so the OOM killer takes slot processes first; a restore whose tasks inherited the server's protection fails
the OOM-priority gate.  Each reset reports the OOM kills the slot's cgroup saw during the episode that just ended
(`last_episode_oom_kills`): an episode with an OOM kill in its slot is invalid even when the reset succeeds.

The images of a branched slot are HARD LINKS of the origin's (same inodes; the server refuses any request whose image files are not the
inodes it was built from), with `stats-restore` unlinked first because CRIU rewrites it on every restore.
"""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import sys
import time

from sharedos_spike import runtime as rt

from . import arm as armmod
from . import launch as L
from . import tmplsrv
from .arm import ArmError
from .branch import COPIED_RECORDS, REQUIRED_RECORDS, BranchArm, ScopedHost, cgroup_kill_backstop
from .candidate import SLOT_MARKER, OwnershipError, source_sha256

POOL_UNIT = "drumsbench-pool.scope"
CG_ROOT = "/sys/fs/cgroup"
POOL = os.path.join(CG_ROOT, "system.slice", POOL_UNIT)
TEMPLATE_CG = os.path.join(POOL, "template")       # before harness/oom-inotify: servers ran here, unprotected (see tmplsrv)
KEEPER_CG = os.path.join(POOL, "keeper")
CLIENT = os.path.join(os.path.dirname(os.path.abspath(__file__)), "pageshare_client.py")
RESTORE_PURPOSE = "criu restore (scope + private mount namespace, overlay as --root)"


def _read(p):
    with open(p) as f:
        return f.read().strip()


def _write(p, v):
    with open(p, "w") as f:
        f.write(v)


def pool_cg(marker: str) -> str:
    return os.path.join(POOL, f"drumsbench-{marker}-pool.scope")


def pool_init() -> dict:
    """The delegated pool scope with a keeper (so systemd never removes it), cpu/memory/pids enabled for its children."""
    if not os.path.isdir(POOL):
        subprocess.run(["systemd-run", "--scope", "--quiet", "--unit", POOL_UNIT, "-p", "Delegate=yes", "--",
                        "setsid", "-f", "sleep", "infinity"], check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(50):
            if os.path.isdir(POOL):
                break
            time.sleep(0.1)
    os.makedirs(KEEPER_CG, exist_ok=True)
    for pid in _read(os.path.join(POOL, "cgroup.procs")).split():
        _write(os.path.join(KEEPER_CG, "cgroup.procs"), pid)
    _write(os.path.join(POOL, "cgroup.subtree_control"), "+cpu +memory +pids")
    os.makedirs(TEMPLATE_CG, exist_ok=True)
    return {"pool": POOL, "keeper_procs": _read(os.path.join(KEEPER_CG, "cgroup.procs")).split(),
            "subtree_control": _read(os.path.join(POOL, "cgroup.subtree_control"))}


def ensure_slot_cg(marker: str, vcpus: int, memory_mib: int) -> str:
    """Create the slot's persistent cgroup once, with the preregistered limits; on reuse, prove the limits are still the same."""
    d = pool_cg(marker)
    want = L.expected_limits(vcpus, memory_mib)
    if not os.path.isdir(d):
        os.mkdir(d)
        _write(os.path.join(d, "cpu.max"), want["cpu_max"])
        _write(os.path.join(d, "memory.max"), want["memory_max"])
        _write(os.path.join(d, "memory.swap.max"), want["swap_max"])
        _write(os.path.join(d, "pids.max"), "8192")
    got = {"cpu_max": _read(os.path.join(d, "cpu.max")), "memory_max": _read(os.path.join(d, "memory.max")),
           "swap_max": _read(os.path.join(d, "memory.swap.max"))}
    if got != want:
        raise ArmError(f"slot cgroup {d} limits {got} differ from the preregistered {want}")
    return d


def empty_slot_cg(d: str, wait_s: float = 10.0) -> dict:
    """After the scoped sweep: the slot's cgroup must hold no process before it is reused; cgroup.kill is the backstop."""
    rep = {"left_after_sweep": [], "cgroup_kill": False}
    if not os.path.isdir(d):
        return rep
    procs = _read(os.path.join(d, "cgroup.procs")).split()
    rep["left_after_sweep"] = procs
    if procs:
        _write(os.path.join(d, "cgroup.kill"), "1")
        rep["cgroup_kill"] = True
    deadline = time.monotonic() + wait_s
    while time.monotonic() < deadline:
        if not _read(os.path.join(d, "cgroup.procs")).split():
            return rep
        time.sleep(0.05)
    raise ArmError(f"slot cgroup {d} still holds processes {_read(os.path.join(d, 'cgroup.procs')).split()} after the sweep and cgroup.kill")


def slot_instance(slot_dir: str):
    """Which incarnation of a slot directory this is: its inode and the created_epoch of its ownership marker. A state reset deletes the
    directory and the next run makes a new one at the same path, with the same tree marker and so the same pool cgroup."""
    try:
        st = os.stat(slot_dir)
        with open(os.path.join(slot_dir, SLOT_MARKER)) as f:
            created = json.load(f).get("created_epoch")
    except (OSError, ValueError, AttributeError):
        return None
    return [st.st_dev, st.st_ino, created]


class PoolScopedHost(ScopedHost):
    """branch.ScopedHost that also accepts the slot's persistent pool cgroup."""

    def __init__(self, slot_marker: str, argv_marker: str = None, **kw):
        super().__init__(slot_marker, argv_marker, **kw)
        self._scope = re.compile(rf"^drumsbench-{re.escape(slot_marker)}(-r\d+|-pool)?\.scope$")


class PageshareArm(BranchArm):
    socket_path = None

    def _ps(self, slot) -> dict:
        try:
            return json.load(open(L.slot_paths(self.cfg["state_dir"], slot)["slot_json"])).get("pageshare") or {}
        except (OSError, ValueError):
            return {}

    def use_slot(self, slot, argv_marker: str = None) -> None:
        self.th = PoolScopedHost(self._marker(slot), argv_marker or self._argv_marker(slot))
        self._cur_slot = slot
        self._pin(slot)

    def _pin(self, slot) -> None:
        """Remember the incarnation of the slot this process works on, the first time it exists (a later use_slot never adopts another)."""
        pins = self.__dict__.setdefault("_pins", {})
        if slot not in pins:
            inst = slot_instance(L.slot_paths(self.cfg["state_dir"], slot)["dir"])
            if inst is not None:
                pins[slot] = inst

    def _replaced(self, slot):
        """None while the slot is the incarnation this process took (or it never took one); else what changed. A process that outlived a
        state reset (its restore hung and held its caller) must not act on the next run's slot of the same number."""
        pin = self.__dict__.get("_pins", {}).get(slot)
        if pin is None:
            return None
        now = slot_instance(L.slot_paths(self.cfg["state_dir"], slot)["dir"])
        return None if now == pin else {"slot": slot, "taken": pin, "now": now}

    def _preserve_logs(self, slot, label):
        if self._replaced(slot):
            return None
        return super()._preserve_logs(slot, label)

    def _oom_audit(self, slot) -> None:
        """Before a slot's tree is stopped: how many of its tasks the OOM killer took during the episode that is ending (the
        oom_kill counter of its persistent cgroup's memory.events, against the value at the end of its last restore).  Recorded in
        slot.json once per episode, so the reset's record names the episode that just ended (None when no episode was open: a
        retry after a failed restore never repeats the count); the counter value is kept as the baseline the next restore is
        checked against."""
        if not self._ps(slot).get("mode"):
            return
        n = tmplsrv.oom_kills(pool_cg(self._marker(slot)))
        if n is None:
            return
        try:
            st = json.load(open(L.slot_paths(self.cfg["state_dir"], slot)["slot_json"]))
        except (OSError, ValueError):
            return
        kw = {"oom_kills_total": n, "restore_oom_baseline": n, "last_episode_oom_kills": None}
        if st.get("episode_open"):
            kw.update(last_episode_oom_kills=n - st.get("oom_kill_baseline", n), episode_open=False)
        self._record(slot, **kw)

    def sweep(self, slot) -> dict:
        replaced = self._replaced(slot)
        if replaced:
            return {"ok": False, "replaced": replaced,
                    "why": f"slot {slot} is not the slot this process took (a state reset and a new run since): nothing was signalled"}
        m = self._marker(slot)
        self._oom_audit(slot)
        host = PoolScopedHost(m, self._argv_marker(slot))
        proxy = rt.sweep_marker(self.ph, m)
        tree = rt.sweep_marker(host, m)
        rep = {"proxy": proxy, "tree": tree, "ok": proxy["ok"] and tree["ok"]}
        if tree["ok"] and not self._ps(slot).get("mode"):
            return rep
        replaced = self._replaced(slot)          # again before any cgroup.kill: the argv sweeps take a while, and cgroup.kill spares nothing
        if replaced:
            return {**rep, "ok": False, "replaced": replaced, "why": f"slot {slot} was replaced during the sweep: its cgroup was not killed"}
        if not tree["ok"]:
            rep["tree_before_cgroup_kill"] = tree
            # 1. the tree survived the per-pid SIGKILL (slow pid-namespace teardown, D state): cgroup.kill its own scope (harness/reset-failures)
            rep["cgroup_kill"] = cgroup_kill_backstop(host, tree)
            if rep["cgroup_kill"]["ok"]:
                rep["tree"] = tree = rt.sweep_marker(host, m)
            if not tree["ok"] and self._ps(slot).get("mode"):
                # 2. a hung template restore: CRIU's forked restorer (no marker argv, so the argv sweep never signals it) still holds the tree
                # and the caller's stdio. The slot's own pool cgroup holds only this slot's processes: empty it, prove again.
                rep["pool_cgroup"] = empty_slot_cg(pool_cg(m))
                rep["tree"] = tree = rt.sweep_marker(host, m)
            rep["ok"] = proxy["ok"] and tree["ok"]
        elif rep["ok"]:
            rep["pool_cgroup"] = empty_slot_cg(pool_cg(m))
        return rep

    def _verify_cgroup(self, slot, pid):
        """The pinned readback, then (pageshare slots) the OOM-priority gate, and no OOM kill in the slot during this restore."""
        info = super()._verify_cgroup(slot, pid)
        if self._ps(slot).get("mode"):
            cg = pool_cg(self._marker(slot))
            gate = tmplsrv.slot_oom_check(cg, pid)
            n = tmplsrv.oom_kills(cg)
            try:
                base = json.load(open(L.slot_paths(self.cfg["state_dir"], slot)["slot_json"])).get("restore_oom_baseline")
            except (OSError, ValueError):
                base = None
            if n is not None and base is not None and n > base:
                raise ArmError(f"the OOM killer took {n - base} task(s) of slot {slot} during this restore (memory.events oom_kill "
                               f"{base} -> {n}): the restored tree is not the checkpoint")
            self._record(slot, oom_gate=gate, oom_kill_baseline=n, restore_oom_baseline=n, episode_open=True)
        return info

    def _run(self, argv, purpose, timeout=600.0, ok=(0,)):
        ps = self._ps(getattr(self, "_cur_slot", -1))
        if purpose == RESTORE_PURPOSE and ps.get("mode"):
            argv = self._rewrite(list(argv), ps)
        return super()._run(argv, purpose, timeout, ok)

    def _rewrite(self, argv, ps) -> list:
        """systemd-run --scope ... -- unshare ... criu restore ARGS  ->  sh (join the slot's pool cgroup) unshare ... [client SOCK] criu restore ARGS"""
        if argv[0] != "systemd-run" or "--" not in argv or "criu" not in argv:
            raise ArmError(f"unexpected restore argv shape: {argv[:12]}")
        rest = argv[argv.index("--") + 1:]
        if rest[0] != "unshare":
            raise ArmError(f"unexpected restore argv after the scope prefix: {rest[:6]}")
        cg = ensure_slot_cg(self._marker(self._cur_slot), self.cfg["vcpus"], self.cfg["memory_mib"])
        i = rest.index("criu")
        if rest[i + 1] != "restore":
            raise ArmError("unexpected criu command in the restore argv")
        if ps["mode"] == "template":
            sock = ps["socket"]
            srv = tmplsrv.require(sock)              # a dead server is restarted (recorded) or this raises: never a silent fallback
            if srv.get("restarted"):
                self._record(self._cur_slot, last_template_restart=srv["event"])
            rest = rest[:i] + [sys.executable, "-I", CLIENT, sock, "criu"] + rest[i + 1:]
        elif ps["mode"] == "pool":
            rest = rest[:i] + [ps.get("criu") or "criu"] + rest[i + 1:]
        return ["sh", "-c", 'echo "$1" > /proc/self/oom_score_adj && echo $$ > "$0/cgroup.procs" && shift && exec "$@"', cg,
                str(tmplsrv.SLOT_OOM_SCORE_ADJ)] + rest

    def branch(self, slot: int, origin: int, mode: str = "template", socket_path: str = None, criu: str = None) -> dict:
        if slot == origin:
            raise ArmError("branch: the target slot must differ from the origin")
        self._require_owned(origin)
        so = self._slot(origin)
        po, p = L.slot_paths(self.cfg["state_dir"], origin), L.slot_paths(self.cfg["state_dir"], slot)
        snap_o = os.path.join(po["dir"], "upper.snap")
        if not os.path.isdir(snap_o) or not os.listdir(po["images"]):
            raise ArmError(f"slot {origin} has no snapshot (upper.snap + images): nothing to branch from")
        for n in REQUIRED_RECORDS:
            if not os.path.isfile(os.path.join(po["dir"], n)):
                raise ArmError(f"slot {origin} lacks {n}: the restore gates would have nothing to compare against")
        if os.path.lexists(p["dir"]):
            raise OwnershipError(f"slot {slot} already exists at {p['dir']}: fresh state only (another slot number)")
        self._require_root_owned()
        os.makedirs(p["dir"], mode=0o700)
        self._write_marker(os.path.join(p["dir"], SLOT_MARKER),
                           {**self._expected_slot_marker(slot), "created_epoch": time.time(), "source_sha256": source_sha256(),
                            "branched_from": {"slot": origin, "tree_marker": self._argv_marker(origin)}}, True)
        self._require_owned(slot)
        t0 = time.monotonic()
        self._run(("cp", "-al", po["images"], p["images"]), "hard-link the origin's images", 1800)
        st = os.path.join(p["images"], "stats-restore")
        if os.path.lexists(st):
            os.unlink(st)
        self._run(("cp", "-a", "--reflink=auto", snap_o, os.path.join(p["dir"], "upper.snap")), "copy the origin's upper.snap", 1800)
        for n in COPIED_RECORDS:
            src = os.path.join(po["dir"], n)
            if os.path.isfile(src):
                shutil.copyfile(src, os.path.join(p["dir"], n))
        for d in ("upper", "work"):
            os.makedirs(os.path.join(p["dir"], d))
        ps = {"mode": mode, "socket": socket_path or self.default_socket(), "criu": criu}
        self._record(slot, port=self._port(slot), file_locks=bool(so.get("file_locks")), state="dumped", restore_generation=0, pageshare=ps,
                     branched_from={"slot": origin, "tree_marker": self._argv_marker(origin), "origin_restore_generation": so.get("restore_generation"),
                                    "copy_seconds": round(time.monotonic() - t0, 3), "epoch": time.time(), "images": "hard links"})
        self.use_slot(slot)
        return self.restore(slot)

    TEMPLATE_REFUSALS = ("not supported with a template", "does not match the template", "The memory template has no entry")

    def restore(self, slot: int) -> dict:
        """Template restore; if the template server refuses this checkpoint (a VMA it cannot template, see criu/mem.c
        tmpl_prepare_mappings) BEFORE any task ran, fall back to a private restore in the same persistent cgroup, and record it.
        Any other failure is not retried."""
        ps = self._ps(slot)
        if ps.get("mode") != "template":
            return super().restore(slot)
        d = L.slot_paths(self.cfg["state_dir"], slot)["dir"]
        t_start = time.time()
        try:
            return super().restore(slot)
        except ArmError as e:
            if self._replaced(slot):
                raise
            logs = sorted((n for n in os.listdir(d) if n.startswith("restore-") and n.endswith(".log")
                           and os.path.getmtime(os.path.join(d, n)) >= t_start - 1), key=lambda n: os.path.getmtime(os.path.join(d, n)))
            txt = open(os.path.join(d, logs[-1]), errors="replace").read() if logs else ""
            why = [l for l in txt.splitlines() if any(r in l for r in self.TEMPLATE_REFUSALS)]
            if not why:
                raise
            fb = {"epoch": time.time(), "template_log": logs[-1], "refusal": why[:3], "error": str(e)[-400:]}
            self._record(slot, pageshare={**ps, "mode": "pool", "template_refused": fb})
            self.use_slot(slot)
            rep = super().restore(slot)
            return self._record(slot, pageshare={**ps, "mode": "pool", "template_refused": fb}, last_restore_fallback=fb)

    def default_socket(self) -> str:
        return os.path.join(os.path.abspath(self.cfg["state_dir"]), "pageshare.sock")

    def serve(self, origin: int, criu: str, socket_path: str = None) -> dict:
        po = L.slot_paths(self.cfg["state_dir"], origin)
        sock = socket_path or self.default_socket()
        pool_init()
        log = os.path.join(os.path.abspath(self.cfg["state_dir"]), f"pageshare-server-{int(time.time())}.log")
        out = log[:-4] + ".out"
        pidf = sock + ".pid" if socket_path else os.path.join(os.path.abspath(self.cfg["state_dir"]), "pageshare-server.pid")
        sv = tmplsrv.start(criu, po["images"], sock, log, out, pidf)
        return {"socket": sock, "ready_s": sv["ready_s"], "out": open(out).read(), "log": log, "server_pid": sv["pid"],
                "template_memory_current": sv["memory_current"], "cgroup": sv["cgroup"], "oom_score_adj": sv["oom_score_adj"],
                "protection": sv["protection"]}


def main(argv: list, executor=None, proxy_host=None) -> int:
    from drumsbench.executor import SubprocessExecutor
    if not argv:
        raise SystemExit(__doc__)
    verb, rest = argv[0], argv[1:]

    def opt(name, default=None):
        return rest[rest.index(name) + 1] if name in rest else default
    try:
        cfg = armmod.load_config(os.environ.get("SHAREDOS_ROOTFS_CONFIG", ""))
        arm = PageshareArm(cfg, executor or SubprocessExecutor(), rt.Host(module=L.TREE_MODULE), proxy_host or rt.Host(module=L.PROXY_MODULE))
        if verb == "pool-init":
            print(json.dumps(pool_init()))
            return 0
        if verb == "serve":
            print(json.dumps(arm.serve(int(opt("--from")), opt("--criu", "criu"), opt("--socket"))))
            return 0
        if verb == "stop-server":
            print(json.dumps(tmplsrv.stop(opt("--socket") or arm.default_socket())))
            return 0
        if verb == "server-status":
            st = tmplsrv.status(opt("--socket") or arm.default_socket())
            print(json.dumps(st))
            return 0 if st["alive"] else 1
        if verb == "drop":
            slot = int(opt("--slot"))
            if slot == 0:
                raise ArmError("drop refuses the origin slot 0")
            arm.use_slot(slot)
            rep = arm.sweep(slot)
            if not rep["ok"]:
                raise ArmError(f"sweep failed: {rep}")
            d = L.slot_paths(cfg["state_dir"], slot)["dir"]
            removed = []
            for sub in ("upper", "upper.snap", "work", "merged", "images"):
                if os.path.isdir(os.path.join(d, sub)):
                    shutil.rmtree(os.path.join(d, sub)); removed.append(sub)
            print(json.dumps({"slot": slot, "sweep_ok": True, "pool_cgroup": rep.get("pool_cgroup"), "removed": removed}))
            return 0
        if verb == "cleanup":
            rep = arm.cleanup()
            print(json.dumps(rep))
            return 0 if rep["ok"] else 1
        if "--slot" not in rest:
            raise ArmError("--slot N required")
        slot = int(opt("--slot"))
        if verb == "branch":
            origin = int(opt("--from"))
            arm.use_slot(slot, arm._argv_marker(origin))
            print(json.dumps(arm.branch(slot, origin, opt("--mode", "template"), opt("--socket"), opt("--criu"))))
        elif verb in ("restore", "reset"):
            arm.use_slot(slot)
            print(json.dumps(getattr(arm, verb)(slot)))
        elif verb == "endpoint":
            arm.use_slot(slot)
            print(json.dumps(arm.endpoint(slot)))
        else:
            raise ArmError(f"unknown verb {verb!r}")
        return 0
    except (ArmError, OSError, ValueError, subprocess.CalledProcessError) as e:
        print(f"arm error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
