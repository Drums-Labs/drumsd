"""drumsbench external-arm shim for the rootfs slot runtime.

    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.arm <verb> [--slot N]

Verbs: describe | environment | provision | endpoint | snapshot | restore | reset | cleanup. Fails closed.
snapshot = CRIU dump (tree stopped) + copy of the overlay upper layer + immediate restore, because CRIU does not
snapshot disk state; restore/reset = stop the tree, put the saved upper layer back, CRIU restore. The
`endpoint` JSON carries the host port of the slot's guest server plus OSWORLD_READY_DISPLAY_PROCS, which tells the
unchanged OSWorld `ready` check which display-server process name this arm legitimately runs.
"""

from __future__ import annotations

import json
import os
import re
import shlex
import shutil
import sys
import time
import urllib.request

from drumsbench.contract import Command
from drumsbench.workload import finite_number
from osworld_smoke import pins

from sharedos_spike import runtime as rt

from . import FIDELITY_DIFFERENCES, PUBLIC_SOURCES
from . import display_stage as ds
from . import launch as L
from .rootfs_inspect import exec_argv

PROFILES = ("gnome_x11", "display_only")
TOOLS = ("criu", "unshare", "nsenter", "setsid", "systemd-run")
HEAVY = ("upper", "upper.snap", "work", "merged", "images")


def _has_setns() -> bool:
    return hasattr(os, "setns")   # Python 3.12+; tests patch this function, never the os module


class ArmError(Exception):
    pass


def load_config(path: str) -> dict:
    try:
        cfg = json.load(open(path))
    except (OSError, ValueError) as e:
        raise ArmError(f"cannot read config {path}: {e}")
    for k in ("state_dir", "lower", "inspection", "image_sha256", "vcpus", "memory_mib", "screen", "profile",
              "service_base_port", "guest_service_port"):
        if k not in cfg:
            raise ArmError(f"config: missing '{k}'")
    for k in ("vcpus", "memory_mib", "service_base_port", "guest_service_port"):
        if isinstance(cfg[k], bool) or not isinstance(cfg[k], int) or cfg[k] <= 0:
            raise ArmError(f"config.{k} must be a positive int")
    if cfg["profile"] not in PROFILES:
        raise ArmError(f"config.profile must be one of {PROFILES}")
    if not re.fullmatch(r"\d{3,5}x\d{3,5}x(24|32)", cfg["screen"]):
        raise ArmError("config.screen must look like 1920x1080x24")
    if "display_stage_deb" in cfg and not (isinstance(cfg["display_stage_deb"], str) and cfg["display_stage_deb"]):
        raise ArmError("config.display_stage_deb must be a path string")
    if cfg["image_sha256"] != pins.IMAGE_QCOW_SHA256:
        raise ArmError("config.image_sha256 is not the pinned OSWorld image: this arm must run the exact same image")
    if not os.path.isdir(cfg["lower"]):
        raise ArmError(f"config.lower {cfg['lower']} is not a directory (is the image mounted read-only?)")
    try:
        cfg["_inspection"] = json.load(open(cfg["inspection"]))
    except (OSError, ValueError) as e:
        raise ArmError(f"cannot read inspection {cfg['inspection']}: {e}")
    if not cfg["_inspection"].get("ok_for_server"):
        raise ArmError(f"inspection is not ok_for_server: {cfg['_inspection'].get('blockers')}")
    if os.path.abspath(cfg["_inspection"].get("root", "")) != os.path.abspath(cfg["lower"]):
        raise ArmError("the inspection was made of a different root than config.lower")
    if any(k not in cfg["_inspection"] for k in ("system_bus", "session", "groups", "hostname", "login")):
        raise ArmError("the inspection predates the system_bus/session/groups/hostname/login facts: regenerate it with `python -m sharedos_rootfs.rootfs_inspect` "
                       "(an old inspection must not be reused)")
    if cfg["_inspection"].get("image_sha256") != pins.IMAGE_QCOW_SHA256:
        raise ArmError("the inspection does not record the pinned image sha256 (inspect with --mount-state from `image attach`): "
                       f"got {cfg['_inspection'].get('image_sha256')!r}")
    return cfg


# Groups whose members the image's sudoers or polkit rules make administrators (sudo, admin, wheel), or that give root
# through a daemon's socket (lxd, docker). The desktop user (the agent) is never in them inside a slot: not in its
# processes' groups, not in the slot's /etc/group and /etc/gshadow (tree.strip_admin_groups). Same list as tree.ADMIN_GROUPS.
ADMIN_GROUPS = ("sudo", "admin", "wheel", "lxd", "docker")


def build_plan(cfg: dict, slot: int, inspection: dict = None) -> dict:
    p = L.slot_paths(cfg["state_dir"], slot)
    insp = inspection or cfg["_inspection"]       # the effective one when a display driver was staged for this slot
    srv = insp["server"]
    launcher = next((x for x in insp["launchers"] if x["exec"]), None)
    argv = exec_argv(launcher["exec"][0]) if launcher else ["python3", srv["path"]]
    cwd = (launcher["working_directory"][0] if launcher and launcher["working_directory"]
           else os.path.dirname(srv["path"]))
    env = {}
    for line in (launcher["environment"] if launcher else []):
        # systemd.exec(5) Environment=: a space-separated list of assignments with shell-like quoting
        # (`Environment="DISPLAY=:0"` assigns DISPLAY; splitting on spaces first made the name `"DISPLAY`)
        for tok in shlex.split(line):
            if "=" in tok:
                k, v = tok.split("=", 1)
                env[k] = v
    u = insp["user"]
    # the agent acts as this user: no group that grants root (sudo, admin, wheel) is applied (no privilege escalation)
    sup = [g for g in insp["groups"]["user_supplementary"] if g["name"] not in ADMIN_GROUPS]
    return {"lower": cfg["lower"], "upper": p["upper"], "work": p["work"], "merged": p["merged"],
            "user": {**{k: u[k] for k in ("name", "uid", "gid", "home", "shell")},
                     # the IMAGE's own supplementary memberships (from its /etc/group), applied under setgroups; never the host's
                     "groups": [g["gid"] for g in sup],
                     "group_names": [g["name"] for g in sup],
                     "dropped_admin_groups": [g["name"] for g in insp["groups"]["user_supplementary"] if g["name"] in ADMIN_GROUPS]},
            "tty_gid": insp["groups"]["tty_gid"], "hostname": insp["hostname"]["etc_hostname"],
            "hostname_mapped_in_hosts": insp["hostname"]["etc_hosts_maps_it"], "display": ":0", "screen": cfg["screen"],
            "profile": cfg["profile"], "guest_service_port": cfg["guest_service_port"],
            "inspection": {**{k: insp[k] for k in ("ok_for_server", "blockers", "binaries", "display", "system_bus", "session")},
                           "login": insp.get("login")},
            "server": {"argv": argv, "cwd": cwd, "env": env},
            # the per-env memory/CPU view (drumsd procview): None = the slot reads the host's files (as before)
            "procview": ({"dir": cfg["procview_dir"], "files": dict(cr_procview_files())} if cfg.get("procview_dir") else None)}


def cr_procview_files():
    from .criu_root import PROCVIEW_FILES
    return PROCVIEW_FILES


def display_procs(cfg: dict) -> str:
    d = cfg["_inspection"]["display"]["decision"]
    if d == "needs_added_display_package" and cfg.get("display_stage_deb"):
        d = "xorg_dummy_staged"
    return {"xvfb_in_image": "Xvfb", "xorg_dummy_in_image": "Xorg", "xorg_dummy_staged": "Xorg"}.get(d, "")


def http_get(url: str, timeout: float = 5.0) -> str:
    with urllib.request.urlopen(url, timeout=timeout) as r:
        return r.read().decode(errors="replace")


def criu_log_evidence(argv, n: int = 6) -> str:
    """For a failed criu command run with `-o LOG`: the last Error lines of LOG. criu writes its diagnosis to that log, not to
    stderr, so without this a failed dump or restore reports only stderr='' (lab-scale-bench layer d, 2026-10-29, lost its cause)."""
    argv = list(argv)
    if "criu" not in [os.path.basename(x) for x in argv] or "-o" not in argv or argv.index("-o") + 1 >= len(argv):
        return ""
    log = argv[argv.index("-o") + 1]
    try:
        with open(log, errors="replace") as f:
            lines = f.read().splitlines()
    except OSError as e:
        return f" criu_log={log} (unreadable: {e})"
    errs = [x.strip() for x in lines if "Error" in x]
    tail = errs[-n:] if errs else [x.strip() for x in lines[-3:]]
    return f" criu_log={log} {'errors' if errs else 'last lines'}: " + " | ".join(tail)[-1500:] + inotify_limit_note(errs)


INOTIFY_SYSCTL = "/proc/sys/fs/inotify/max_user_instances"


def inotify_limit_note(errs) -> str:
    """A restore that hit the per-user inotify instance limit says so, with the host's value: criu creates every restored
    inotify instance as uid 0, so all slots on the host share ONE count (oom-inotify 20261030T0344Z). The fix is a host
    setting (sharedos_rootfs/host/60-drums-slots.conf), not a retry."""
    if not any("inotify" in e and "Too many open files" in e for e in errs):
        return ""
    try:
        with open(INOTIFY_SYSCTL) as f:
            v = f.read().strip()
    except OSError:
        v = "unreadable"
    return (f" | host inotify instance limit reached: fs.inotify.max_user_instances={v}; restored instances are all charged to "
            "uid 0, one count for every slot on the host; install sharedos_rootfs/host/60-drums-slots.conf")


class Arm:
    def __init__(self, cfg: dict, executor, tree_host, proxy_host, sleep=time.sleep, http=http_get, wait_s: float = 300.0,
                 clock=time.monotonic, controllers_path: str = "/sys/fs/cgroup/cgroup.controllers", cgroup_reader=None, userns_host=None):
        self.cfg, self.ex, self.th, self.ph, self.sleep, self.http, self.wait_s, self.clock = \
            cfg, executor, tree_host, proxy_host, sleep, http, wait_s, clock
        self.controllers_path = controllers_path
        self.cgroup_reader = cgroup_reader or L.read_cgroup
        from . import harden
        self.userns = userns_host or harden.HostUserns()     # host-root preparation for a slot in its own user namespace

    # plumbing -----------------------------------------------------------------------------------------
    def _run(self, argv, purpose, timeout=600.0, ok=(0,)):
        res = self.ex.run(Command("arm", tuple(argv), purpose, timeout_s=timeout), timeout)
        if res.rc not in ok or res.error:
            raise ArmError(f"{purpose} failed rc={res.rc} error={res.error} stderr={res.stderr[-400:]!r}{criu_log_evidence(argv)}")
        return res

    def _marker(self, slot):
        return L.marker_for(self.cfg["state_dir"], slot)

    def _port(self, slot):
        return self.cfg["service_base_port"] + slot

    def _record(self, slot, **kw):
        p = L.slot_paths(self.cfg["state_dir"], slot)["slot_json"]
        cur = json.load(open(p)) if os.path.exists(p) else {}
        cur.update(kw)
        os.makedirs(os.path.dirname(p), exist_ok=True)
        json.dump(cur, open(p, "w"), indent=2)
        return cur

    def _slot(self, slot):
        p = L.slot_paths(self.cfg["state_dir"], slot)["slot_json"]
        if not os.path.exists(p):
            raise ArmError(f"slot {slot} was never provisioned")
        return json.load(open(p))

    def sweep(self, slot) -> dict:
        m = self._marker(slot)
        proxy = rt.sweep_marker(self.ph, m)
        tree = rt.sweep_marker(self.th, m)
        return {"proxy": proxy, "tree": tree, "ok": proxy["ok"] and tree["ok"]}

    def _preserve_logs(self, slot, label):
        """Copy the slot's service logs (they live in the upper layer, which cleanup deletes) and the tree log next to
        slot.json so a failed start keeps its evidence. Files only; never raises."""
        p = L.slot_paths(self.cfg["state_dir"], slot)
        src = os.path.join(p["upper"], "var/log/sharedos")
        dest = os.path.join(p["dir"], f"service-logs-{label}")
        try:
            if not os.path.isdir(src) or os.path.exists(dest):
                return None
            os.makedirs(dest)
            for n in sorted(os.listdir(src)):
                f = os.path.join(src, n)
                if os.path.isfile(f) and not os.path.islink(f):
                    shutil.copyfile(f, os.path.join(dest, n))
            return dest
        except OSError:
            return None

    def _verify_cgroup(self, slot, pid):
        """Read back the slot's cgroup and limits from the kernel BEFORE anything declares the slot ready. Anything
        unreadable or different fails closed (the caller sweeps the slot)."""
        try:
            info = self.cgroup_reader(pid)
        except OSError as e:
            raise ArmError(f"cannot read back the cgroup of init pid {pid}: {e}")
        bad = L.verify_cgroup(info, pid, self._marker(slot), self.cfg["vcpus"], self.cfg["memory_mib"])
        if bad:
            raise ArmError(f"slot {slot} is not under the preregistered CPU/memory limits: {bad}")
        self._record(slot, cgroup=info)
        return info

    def _wait_server(self, slot):
        deadline = self.clock() + self.wait_s
        last = None
        while self.clock() < deadline:
            try:
                body = self.http(f"http://127.0.0.1:{self._port(slot)}/platform")
                if body.strip() == "Linux":
                    return
                last = f"unexpected /platform reply {body!r}"
            except OSError as e:
                last = str(e)
            self.sleep(1.0)
        raise ArmError(f"guest server not answering through the proxy on port {self._port(slot)}: {last}")

    def _start_proxy(self, slot, init_pid):
        p = L.slot_paths(self.cfg["state_dir"], slot)
        self._run(L.proxy_argv(self._marker(slot), init_pid, self._port(slot), self.cfg["guest_service_port"], p["proxy_log"]),
                  "start netns proxy", 60)
        for _ in range(50):
            if self.ph.find_marker_procs(self._marker(slot)):
                return
            self.sleep(0.1)
        raise ArmError("proxy process never appeared")

    def _wait_init(self, slot):
        deadline = self.clock() + self.wait_s
        while self.clock() < deadline:
            pid = self.th.find_init(self._marker(slot))
            if pid:
                return pid
            self.sleep(0.5)
        raise ArmError("slot init not found (exact argv, pid 1 in its namespace)")

    # verbs --------------------------------------------------------------------------------------------
    def provision(self, slot: int) -> dict:
        s = self.sweep(slot)
        if not s["ok"]:
            raise ArmError(f"leftover processes for slot {slot} could not be cleared: {s}")
        p = L.slot_paths(self.cfg["state_dir"], slot)
        for d in HEAVY:
            if os.path.exists(os.path.join(p["dir"], d)) and os.listdir(os.path.join(p["dir"], d)):
                raise ArmError(f"{p['dir']}/{d} is not empty: refusing to reuse unknown state (run cleanup)")
        for d in ("upper", "work", "merged", "images"):
            os.makedirs(os.path.join(p["dir"], d), exist_ok=True)
        from . import harden
        hardened = harden.enabled(self.cfg)
        base = harden.subid_base(slot)
        insp = self._stage_display(slot, p)
        plan = build_plan(self.cfg, slot, insp)
        plan["harden"] = hardened
        if hardened:
            # the slot runs in its own user namespace (launch.launch_argv): its lower layer is an idmapped view of the image, files the
            # harness staged into the upper layer get the slot's ids, and the slot's mapped root may enter its own dir (harden.py)
            plan["lower"] = self.userns.idmapped_lower(self.cfg["lower"], base)
            plan["userns"] = {"base": base, "count": harden.SUBID_COUNT}
            self.userns.shift_owner(p["upper"], base)
        from .tree import services_plan, PlanError
        try:
            services_plan(plan)    # fail closed before anything is launched
        except PlanError as e:
            raise ArmError(f"service plan refused: {e}")
        json.dump(plan, open(p["plan"], "w"), indent=2)
        if hardened:
            self.userns.prepare_access(os.path.abspath(self.cfg["state_dir"]), p["dir"], p["plan"], (p["upper"], p["work"], p["merged"]), base)
        if os.path.exists(p["tree_log"]):    # never append a new start to an old failure's traceback; keep the old one
            os.rename(p["tree_log"], f"{p['tree_log']}.{int(time.time())}")
        self._run(L.launch_argv(self._marker(slot), p["plan"], p["tree_log"], self.cfg["vcpus"], self.cfg["memory_mib"], slot,
                                harden=hardened),
                  "launch slot (cgroup scope + namespaces)", 120)
        try:
            pid = self._wait_init(slot)
            self._verify_cgroup(slot, pid)       # limits proven before the proxy or the server check run
            self._start_proxy(slot, pid)
            self._wait_server(slot)
        except BaseException:
            self._preserve_logs(slot, f"provision-{int(time.time())}")   # before anything removes the upper layer
            self.sweep(slot)    # never leave a half-started slot behind
            raise
        return self._record(slot, init_pid=pid, port=self._port(slot), state="running")

    def _stage_display(self, slot, p):
        """The effective inspection for this slot. If the image has no usable headless display and a pinned deb was
        configured, stage its dummy driver into the slot's UPPER layer (never the image) and say so in the inspection;
        otherwise the image's own inspection is used unchanged. Anything unproven fails closed."""
        insp, deb = self.cfg["_inspection"], self.cfg.get("display_stage_deb")
        needs = insp["display"]["decision"] == "needs_added_display_package"
        if not needs:
            if deb:
                raise ArmError("config.display_stage_deb is set but the image has its own headless display: refusing to add anything")
            return insp
        if not deb:
            raise ArmError("the image has no headless display (needs_added_display_package): set config.display_stage_deb to the "
                           "pinned jammy xserver-xorg-video-dummy deb (see display_stage.PIN); nothing is added silently")
        try:
            rec = ds.stage(self.cfg["lower"], deb, p["upper"], p["dir"])
            eff = ds.inspect_staged(insp, rec)
        except ds.StageError as e:
            raise ArmError(f"display driver staging refused (fail closed): {e}")
        self._record(slot, display_stage=rec)
        return eff

    def endpoint(self, slot: int) -> dict:
        self._slot(slot)
        env = {"BENCH_SERVICE_PORT": self._port(slot), "BENCH_SLOT": slot}
        dp = display_procs(self.cfg)
        if dp:
            env["OSWORLD_READY_DISPLAY_PROCS"] = dp
        return env

    def _restore(self, slot, label):
        p = L.slot_paths(self.cfg["state_dir"], slot)
        pidfile = os.path.join(p["dir"], "restored.pid")
        if os.path.exists(pidfile):
            os.remove(pidfile)
        gen = self._slot(slot).get("restore_generation", 0) + 1
        self._record(slot, restore_generation=gen)
        # criu runs INSIDE a fresh constrained scope so the restored tasks cannot escape the 4 vCPU / 4 GiB limits
        self._run(L.restore_scoped_argv(self._marker(slot), gen, self.cfg["vcpus"], self.cfg["memory_mib"], p["images"],
                                        pidfile, os.path.join(p["dir"], f"restore-{label}-{int(time.time())}.log")),
                  "criu restore in a constrained scope", 600)
        try:
            pid = self.th.read_pid(pidfile)
            if pid is None:
                raise ArmError("criu restore wrote no pidfile")
            self._verify_cgroup(slot, pid)       # before the proxy and before anything can call the slot ready
            self._start_proxy(slot, pid)
            self._wait_server(slot)
        except BaseException:
            self._preserve_logs(slot, f"restore-{label}-{int(time.time())}")
            self.sweep(slot)    # restored but unverified/unreachable: do not leave it running unconstrained
            raise
        return self._record(slot, init_pid=pid, state="running")

    def snapshot(self, slot: int) -> dict:
        """dump (stops the tree) -> copy upper -> restore: a consistent process+disk snapshot, and the first restore."""
        st = self._slot(slot)
        p = L.slot_paths(self.cfg["state_dir"], slot)
        if os.path.exists(os.path.join(p["dir"], "upper.snap")):
            raise ArmError("upper.snap already exists: a slot has one snapshot; run cleanup first")
        for x in os.listdir(p["images"]):
            raise ArmError("images dir is not empty: refusing to overwrite an existing dump")
        self._run(L.dump_argv(st["init_pid"], p["images"], os.path.join(p["dir"], "dump.log")), "criu dump (stops the tree)", 900)
        if not os.listdir(p["images"]):
            raise ArmError("criu dump wrote no images")
        stale = rt.sweep_marker(self.ph, self._marker(slot))   # the proxy still points at the dumped init
        if not stale["ok"]:
            raise ArmError(f"cannot stop the proxy before restoring: {stale}")
        self._run(("cp", "-a", "--reflink=auto", p["upper"], os.path.join(p["dir"], "upper.snap")), "copy upper layer", 1800)
        self._record(slot, state="dumped")
        return self._restore(slot, "post-snapshot")

    def restore(self, slot: int) -> dict:
        """Stop whatever runs, put the saved upper layer back, CRIU restore. This is also `reset`."""
        self._slot(slot)
        p = L.slot_paths(self.cfg["state_dir"], slot)
        snap = os.path.join(p["dir"], "upper.snap")
        if not os.path.isdir(snap) or not os.listdir(p["images"]):
            raise ArmError("no snapshot (upper.snap + images) for this slot")
        s = self.sweep(slot)
        if not s["ok"]:
            raise ArmError(f"cannot stop slot {slot}: {s}")
        for sub in ("upper", "work"):
            shutil.rmtree(os.path.join(p["dir"], sub), ignore_errors=False)
        os.makedirs(os.path.join(p["dir"], "work"))
        self._run(("cp", "-a", "--reflink=auto", snap, p["upper"]), "put saved upper layer back", 1800)
        return self._restore(slot, "restore")

    reset = restore

    def cleanup(self) -> dict:
        base = os.path.abspath(self.cfg["state_dir"])
        out = {"ok": True, "slots": {}}
        if not os.path.isdir(base):
            return out
        for name in sorted(os.listdir(base)):
            if not (name.startswith("slot") and name[4:].isdigit()):
                continue
            slot = int(name[4:])
            s = self.sweep(slot)
            rec = {"sweep_ok": s["ok"], "removed": []}
            if s["ok"]:
                p = L.slot_paths(self.cfg["state_dir"], slot)
                self._preserve_logs(slot, "at-cleanup")        # the upper layer is about to be deleted
                for d in HEAVY:
                    path = os.path.join(p["dir"], d)
                    if os.path.isdir(path) and os.path.abspath(path).startswith(base + os.sep):
                        mounted = self.ex.run(Command("arm", ("findmnt", "-n", path), "mounted?"), 30).rc == 0
                        if mounted:
                            rec["sweep_ok"] = False
                            rec["why"] = f"{path} is still a mount point: not deleted"
                            continue
                        shutil.rmtree(path)
                        rec["removed"].append(d)
            else:
                rec["evidence"] = s
            out["slots"][name] = rec
            out["ok"] = out["ok"] and rec["sweep_ok"]
        return out

    def environment(self) -> str:
        missing = [t for t in TOOLS if not self.th.which(t)]
        if missing:
            raise ArmError(f"missing tools: {missing}")
        if not _has_setns():
            raise ArmError("Python 3.12+ (os.setns) is required for the netns proxy")
        ctl = self.controllers_path
        if not os.path.exists(ctl) or not {"cpu", "memory"} <= set(open(ctl).read().split()):
            raise ArmError("cgroup v2 with cpu and memory controllers is required for the resource limits")
        lines = [f"kernel: {os.uname().release}", f"cgroup controllers: {open(ctl).read().strip()}"]
        for t in ("criu", "unshare", "systemd-run"):
            r = self.ex.run(Command("arm", (t, "--version"), f"{t} version"), 30)
            lines.append(f"{t}: {(r.stdout or r.stderr).strip().splitlines()[0] if (r.stdout or r.stderr).strip() else '?'}")
        return "\n".join(lines)

    def describe(self) -> dict:
        c = self.cfg
        return {"arm": "sharedos-rootfs", "image_sha256": c["image_sha256"], "profile": c["profile"],
                "resources": {"vcpus": c["vcpus"], "memory_mib": c["memory_mib"],
                              "cgroup_properties": L.cgroup_properties(c["vcpus"], c["memory_mib"])},
                "screen": c["screen"], "service_base_port": c["service_base_port"],
                "guest_service_port": c["guest_service_port"],
                "display": c["_inspection"]["display"],
                "display_stage": {"configured": bool(c.get("display_stage_deb")), "pin": {k: ds.PIN[k] for k in ("package", "version", "deb_sha256", "driver_sha256", "depends")}},
                "server": c["_inspection"]["server"],
                "server_matches_pinned_public_source": c["_inspection"]["server_matches_pinned_public_source"],
                "user": c["_inspection"]["user"]["name"],
                "launch_argv_template": list(L.launch_argv("MARKER", "PLAN", "LOG", c["vcpus"], c["memory_mib"])),
                "criu_dump_argv_template": list(L.dump_argv(0, "IMAGES", "LOG")),
                "criu_restore_argv_template": list(L.restore_scoped_argv("MARKER", 1, c["vcpus"], c["memory_mib"], "IMAGES",
                                                                       "PIDFILE", "LOG")),
                "inspection_image_sha256": c["_inspection"]["image_sha256"],
                "fidelity_differences": FIDELITY_DIFFERENCES, "public_sources": PUBLIC_SOURCES}


def main(argv: list, executor=None, tree_host=None, proxy_host=None) -> int:
    from drumsbench.executor import SubprocessExecutor
    if not argv:
        raise SystemExit(__doc__)
    verb, rest = argv[0], argv[1:]
    try:
        cfg = load_config(os.environ.get("SHAREDOS_ROOTFS_CONFIG", ""))
        arm = Arm(cfg, executor or SubprocessExecutor(), tree_host or rt.Host(module=L.TREE_MODULE),
                  proxy_host or rt.Host(module=L.PROXY_MODULE))
        if verb == "describe":
            print(json.dumps(arm.describe(), sort_keys=True, indent=2))
        elif verb == "environment":
            print(arm.environment())
        elif verb == "cleanup":
            rep = arm.cleanup()
            print(json.dumps(rep))
            return 0 if rep["ok"] else 1
        else:
            if "--slot" not in rest:
                raise ArmError("--slot N required")
            slot = int(rest[rest.index("--slot") + 1])
            if verb == "endpoint":
                print(json.dumps(arm.endpoint(slot)))
            elif verb in ("provision", "snapshot", "restore", "reset"):
                getattr(arm, verb)(slot)
            else:
                raise ArmError(f"unknown verb {verb!r}")
        return 0
    except (ArmError, OSError, ValueError) as e:
        print(f"arm error: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
