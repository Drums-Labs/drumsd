"""Branch: restore ONE slot's checkpoint into ANOTHER slot of the same candidate state dir, so two slots run two independent
continuations of one prepared state on one host at the same time (CANDIDATE.md section 6, "a second slot for isolation").

    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.branch branch --slot T --from S
    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.branch <restore|reset|endpoint> --slot N
    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.branch cleanup

What the pinned arm (candidate.py, arm.py, sharedos_spike/runtime.py at 91988fc) cannot do by itself, read from its source:
  * `snapshot`, `restore` and `reset` only ever use the slot's OWN `images` and `upper.snap`; no verb restores slot S's checkpoint into slot T.
  * A slot's processes are found by the tree marker baked into the init's argv (`-m sharedos_rootfs.tree --marker rootfs-<h>-<slot>`):
    runtime.Host.find_init / find_all_inits / find_marker_procs / terminate scan EVERY host process for that exact argv. A tree restored
    from S's checkpoint carries S's marker in its argv wherever it is restored, so with two such trees alive `find_init(S)` sees two inits
    (and returns None) and `sweep_marker(S)` would stop BOTH trees: a reset of slot S would kill slot T.

The minimum change, with the pinned sources untouched (candidate.SOURCE_FILES and their hash are unchanged, so the checkpoint is still one
the pinned code made and the pinned code's own `restore` is what restores it):
  1. `ScopedHost`: runtime.Host restricted to the processes inside ONE slot's own cgroup scopes (`drumsbench-<slot marker>.scope` at
     provision, `drumsbench-<slot marker>-r<gen>.scope` after each restore; both names are chosen by the harness at launch/restore time,
     never taken from the image), reading the origin's marker in a restored argv as the slot's own. Exact-argv matching, "pid 1 of its own
     pid namespace", and the pidfd-pinned signalling are inherited unchanged.
  2. `BranchArm.branch(T, S)`: T must not exist; S must be owned and dumped (`upper.snap`, non-empty `images`, the three records the restore
     gates compare against). T gets its own ownership marker (same source hash), a COPY of S's images and upper.snap (two restores must not
     share an images dir, CRIU writes its stats there, and the upper layer is per-slot disk state), the dumped network/mount/lock records,
     and a slot.json naming the origin; then the pinned `restore` verb runs unchanged (sweep of T, upper from upper.snap, criu restore under
     --root in T's own overlay inside a private mount namespace, scope drumsbench-<T marker>-r1, lock/cgroup/mount/net gates, T's own proxy
     and port).
"""

from __future__ import annotations

import json
import os
import re
import shutil
import sys
import time

from sharedos_spike import runtime as rt

from . import arm as armmod
from . import launch as L
from .arm import ArmError
from .candidate import SLOT_MARKER, CandidateArm, OwnershipError, source_sha256

REQUIRED_RECORDS = ("netstate-pre-dump.json", "mounts-pre-dump.json", "locks-dumped.json")
COPIED_RECORDS = REQUIRED_RECORDS + ("locks-census.json", "images-manifest.json", "dump.log", "service-plan.json")


CG_ROOT = "/sys/fs/cgroup"


def _populated(d: str) -> bool:
    try:
        with open(os.path.join(d, "cgroup.events")) as f:
            return any(line.split() == ["populated", "1"] for line in f)
    except OSError:
        return False          # the cgroup is gone: nothing left in it


def cgroup_kill_backstop(host, tree_rep: dict, cg_root: str = CG_ROOT, wait_s: float = 30.0, sleep=time.sleep,
                         clock=time.monotonic) -> dict:
    """The scoped sweep left a tree process alive ('still alive after SIGKILL', e.g. a slow pid-namespace teardown, a D-state task or
    a frozen cgroup). SIGKILL every task of the slot's OWN cgroup at once with cgroup.kill (kernel-side; it also reaches tasks the pid
    walk cannot name) and require the cgroup to be empty within wait_s. Only cgroups whose name matches this slot's scope pattern
    (host._scope) are touched; anything else is reported, never killed. Exp3 criufix run 2026-10-28 08:54:43Z: 16 of 16 slots."""
    pids = [f["pid"] for f in tree_rep.get("failures", [])] + list(tree_rep.get("remaining", []))
    cgs, foreign = set(), []
    for pid in pids:
        try:
            with open(f"{host.proc}/{pid}/cgroup") as f:
                lines = [x for x in f.read().splitlines() if x.startswith("0::")]
        except OSError:
            continue
        if len(lines) == 1 and host._scope.match(os.path.basename(lines[0][3:])):
            cgs.add(lines[0][3:])
        else:
            foreign.append({"pid": pid, "cgroup": lines})
    out = {"cgroups": sorted(cgs), "killed": [], "errors": [], "foreign": foreign}
    for cg in sorted(cgs):
        try:
            with open(os.path.join(cg_root, cg.lstrip("/"), "cgroup.kill"), "w") as f:
                f.write("1")
            out["killed"].append(cg)
        except OSError as e:
            out["errors"].append(f"{cg}: {e}")
    t0 = clock()
    while True:
        left = [cg for cg in sorted(cgs) if _populated(os.path.join(cg_root, cg.lstrip("/")))]
        if not left or clock() - t0 >= wait_s:
            break
        sleep(0.1)
    out["still_populated"], out["seconds"] = left, round(clock() - t0, 3)
    out["ok"] = bool(cgs) and not left and not out["errors"] and not foreign
    return out


def sweep_with_backstop(proxy_host, tree_host, marker: str, **kw) -> dict:
    """rt.sweep_marker for proxy and tree; if the tree is not proven stopped, cgroup.kill its own cgroup and sweep it again."""
    proxy = rt.sweep_marker(proxy_host, marker)
    tree = rt.sweep_marker(tree_host, marker)
    rep = {"proxy": proxy, "tree": tree}
    if not tree["ok"]:
        rep["cgroup_kill"] = cgroup_kill_backstop(tree_host, tree, **kw)
        if rep["cgroup_kill"]["ok"]:
            rep["tree_after_cgroup_kill"] = tree = rt.sweep_marker(tree_host, marker)
    rep["ok"] = proxy["ok"] and tree["ok"]
    return rep


class ScopedHost(rt.Host):
    """runtime.Host that sees only the processes of ONE slot's cgroup scopes, and reads the origin's tree marker as the slot's own."""

    def __init__(self, slot_marker: str, argv_marker: str = None, proc: str = "/proc", module: str = L.TREE_MODULE):
        super().__init__(proc=proc, module=module)
        self.slot_marker = slot_marker
        self.argv_marker = argv_marker or slot_marker
        self._scope = re.compile(rf"^drumsbench-{re.escape(slot_marker)}(-r\d+)?\.scope$")

    def in_scope(self, pid) -> bool:
        try:
            with open(f"{self.proc}/{pid}/cgroup") as f:
                lines = [x for x in f.read().splitlines() if x.startswith("0::")]
        except OSError:
            return False
        return len(lines) == 1 and bool(self._scope.match(os.path.basename(lines[0][3:])))

    def _argv(self, pid):
        """None (no match possible) for any process outside this slot's scopes; otherwise the argv with the origin's marker, if the slot
        runs a branched tree, read as the slot's own marker so the inherited exact-argv checks identify it."""
        if not self.in_scope(pid):
            return None
        argv = super()._argv(pid)
        if argv and self.argv_marker != self.slot_marker:
            argv = [self.slot_marker if (x == self.argv_marker and i > 0 and argv[i - 1] == "--marker") else x for i, x in enumerate(argv)]
        return argv


class BranchArm(CandidateArm):
    def _argv_marker(self, slot) -> str:
        """The marker in the slot's tree argv: the origin's for a branched slot (recorded in slot.json), else the slot's own."""
        p = L.slot_paths(self.cfg["state_dir"], slot)["slot_json"]
        try:
            with open(p) as f:
                origin = json.load(f).get("branched_from") or {}
        except (OSError, ValueError):
            origin = {}
        return origin.get("tree_marker") or self._marker(slot)

    def use_slot(self, slot, argv_marker: str = None) -> None:
        self.th = ScopedHost(self._marker(slot), argv_marker or self._argv_marker(slot))

    def sweep(self, slot) -> dict:
        """Per-slot scoped sweep (cleanup walks every slot with one arm instance, so the tree host is built per slot here)."""
        m = self._marker(slot)
        return sweep_with_backstop(self.ph, ScopedHost(m, self._argv_marker(slot)), m)

    def branch(self, slot: int, origin: int) -> dict:
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
        self._run(("cp", "-a", "--reflink=auto", po["images"], p["images"]), "copy the origin's images", 1800)
        self._run(("cp", "-a", "--reflink=auto", snap_o, os.path.join(p["dir"], "upper.snap")), "copy the origin's upper.snap", 1800)
        for n in COPIED_RECORDS:
            src = os.path.join(po["dir"], n)
            if os.path.isfile(src):
                shutil.copyfile(src, os.path.join(p["dir"], n))
        for d in ("upper", "work"):
            os.makedirs(os.path.join(p["dir"], d))
        self._record(slot, port=self._port(slot), file_locks=bool(so.get("file_locks")), state="dumped", restore_generation=0,
                     branched_from={"slot": origin, "tree_marker": self._argv_marker(origin), "origin_restore_generation": so.get("restore_generation"),
                                    "copy_seconds": round(time.monotonic() - t0, 3), "epoch": time.time()})
        self.use_slot(slot)
        return self.restore(slot)


def main(argv: list, executor=None, proxy_host=None) -> int:
    from drumsbench.executor import SubprocessExecutor
    if not argv:
        raise SystemExit(__doc__)
    verb, rest = argv[0], argv[1:]
    try:
        cfg = armmod.load_config(os.environ.get("SHAREDOS_ROOTFS_CONFIG", ""))
        arm = BranchArm(cfg, executor or SubprocessExecutor(), rt.Host(module=L.TREE_MODULE), proxy_host or rt.Host(module=L.PROXY_MODULE))
        if verb == "cleanup":
            rep = arm.cleanup()
            print(json.dumps(rep))
            return 0 if rep["ok"] else 1
        if "--slot" not in rest:
            raise ArmError("--slot N required")
        slot = int(rest[rest.index("--slot") + 1])
        if verb == "branch":
            if "--from" not in rest:
                raise ArmError("branch needs --from S")
            origin = int(rest[rest.index("--from") + 1])
            arm.use_slot(slot, arm._argv_marker(origin))
            print(json.dumps(arm.branch(slot, origin)))
        elif verb in ("restore", "reset"):
            arm.use_slot(slot)
            print(json.dumps(getattr(arm, verb)(slot)))
        elif verb == "endpoint":
            arm.use_slot(slot)
            print(json.dumps(arm.endpoint(slot)))
        else:
            raise ArmError(f"unknown verb {verb!r} (provision and snapshot stay with sharedos_rootfs.candidate)")
        return 0
    except (ArmError, OSError, ValueError) as e:
        print(f"arm error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
