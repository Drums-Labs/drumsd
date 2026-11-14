"""The Drums CANDIDATE arm: the custom shared-OS rootfs runtime (tree init, pivot_root, image-side supervisor, CRIU called directly) with
the restore-root contract of criu_root.py, strict ownership of its state, and a network-fidelity gate. See CANDIDATE.md for the boundary
against the OCI reference control. Nothing here has been run on a real host; tests use labelled fakes.

    SHAREDOS_ROOTFS_CONFIG=config.json python -m sharedos_rootfs.candidate <verb> [--slot N]

Differences from the plain `sharedos_rootfs.arm.Arm` (which stays as it was): fresh-state-only provisioning with ownership markers checked
by EVERY verb; the dump is refused unless the init's mount table has no external mounts and the restore always runs under `--root` in a
private mount namespace; the dumped network namespace must be in the images and the restored one must equal the checkpointed one before
the proxy starts; the runtime source hash recorded at provision must match at snapshot/restore; cleanup touches only owned slots.
"""

from __future__ import annotations

import hashlib
import json
import os
import shlex
import shutil
import stat
import sys
import time

from drumsbench.contract import Command
from sharedos_spike import runtime as rt

from . import arm as armmod
from . import criu_root as cr
from . import launch as L
from . import locks as lk
from .arm import ArmError

OWNER_RUNTIME = "sharedos-rootfs-candidate"
OWNER_PROTOCOL = 1
ROOT_MARKER = ".sharedos-candidate-owner.json"
SLOT_MARKER = ".candidate-owner.json"
NET_IMAGE_KINDS = ("netns", "netdev", "ifaddr", "route", "route6", "rule", "iptables", "ip6tables")
REQUIRED_NET_IMAGES = ("netns", "netdev", "ifaddr")      # a full private checkpoint of the namespace must contain at least these
SOURCE_FILES = ("tree.py", "supervisor.py", "launch.py", "criu_root.py", "locks.py", "candidate.py", "proxy.py")
TOOLS = armmod.TOOLS + ("ip", "findmnt")
CRIU_DEFAULT_CONFIGS = ("/etc/criu/default.conf",)


class OwnershipError(ArmError):
    """The state directory or slot is not (provably) the candidate's: nothing is read, copied, swept, unmounted or deleted."""


# ------------------------------------------------------------------------------------------------- pure helpers
def source_sha256(pkg_dir: str = None) -> str:
    """One digest over the runtime sources that create and restore a slot (names and bytes, in a fixed order)."""
    d = pkg_dir or os.path.dirname(os.path.abspath(__file__))
    h = hashlib.sha256()
    for n in SOURCE_FILES:
        with open(os.path.join(d, n), "rb") as f:
            h.update(n.encode() + b"\0" + f.read() + b"\0")
    return h.hexdigest()


def criu_config_files(home, env, global_dir="/etc/criu"):
    blocking = [p for p in (os.path.join(global_dir, "default.conf"), os.path.join(home, ".criu", "default.conf") if home else None)
                if p and os.path.lexists(p)]
    if env.get("CRIU_CONFIG_FILE"):
        blocking.append("$CRIU_CONFIG_FILE=" + env["CRIU_CONFIG_FILE"])
    return blocking


def normalize_net(links, addrs, routes) -> dict:
    """Comparable view of a network namespace from `ip -j link/addr/route show` (volatile LOWER_UP/operstate ignored)."""
    out = {"links": {}, "addresses": [], "routes": []}
    for l in links:
        out["links"][l["ifname"]] = {"ifindex": l.get("ifindex"), "mtu": l.get("mtu"), "up": "UP" in (l.get("flags") or [])}
    for a in addrs:
        for i in a.get("addr_info", []):
            out["addresses"].append([a["ifname"], i.get("family"), i.get("local"), i.get("prefixlen"), i.get("scope")])
    for r in routes:
        out["routes"].append([r.get("dst"), r.get("dev"), r.get("gateway")])
    out["addresses"].sort(key=str)
    out["routes"].sort(key=str)
    return out


def loopback_ok(net: dict) -> bool:
    lo = net["links"].get("lo")
    return bool(lo and lo["up"] and ["lo", "inet", "127.0.0.1", 8, "host"] in net["addresses"])


def diff_net(want: dict, got: dict) -> list:
    d = []
    for name in sorted(set(want["links"]) | set(got["links"])):
        if want["links"].get(name) != got["links"].get(name):
            d.append(f"link {name}: before {want['links'].get(name)} after {got['links'].get(name)}")
    for k in ("addresses", "routes"):
        for item in want[k]:
            if item not in got[k]:
                d.append(f"{k[:-2]} missing after restore: {item}")
        for item in got[k]:
            if item not in want[k]:
                d.append(f"{k[:-2]} appeared after restore: {item}")
    return d


def net_image_manifest(images: str) -> dict:
    names = sorted(n for n in os.listdir(images) if os.path.isfile(os.path.join(images, n)) and not os.path.islink(os.path.join(images, n)))
    sizes = {n: os.path.getsize(os.path.join(images, n)) for n in names}
    net = {k: sorted(n for n in names if n.startswith(k + "-")) for k in NET_IMAGE_KINDS}
    return {"images": sizes, "network_images": net, "missing_required": [k for k in REQUIRED_NET_IMAGES if not net[k]]}


# ------------------------------------------------------------------------------------------------- the arm
class CandidateArm(armmod.Arm):
    def __init__(self, *a, start_time=None, read_text=None, ns_inode=None, host_ns_inode=None, listdir=None, readlink=None, walk=None,
                 ns_members=None, **kw):
        super().__init__(*a, **kw)
        self.listdir, self.readlink = listdir or os.listdir, readlink or os.readlink
        self.walk = walk or (lambda pid: lk.children_walk(pid))
        self.ns_members = ns_members or (lambda pid: lk.pid_ns_members(pid))
        self.start_time = start_time or (lambda pid: L._start_time(pid, "/proc"))
        self.read_text = read_text or (lambda path: open(path).read())
        self.ns_inode = ns_inode or (lambda pid: os.stat(f"/proc/{pid}/ns/net").st_ino)
        self.host_ns_inode = host_ns_inode or (lambda: os.stat("/proc/self/ns/net").st_ino)

    # ----------------------------------------------------------------------------------------- ownership
    def _state_real(self):
        return os.path.realpath(self.cfg["state_dir"])

    @staticmethod
    def _no_links(base, *parts):
        cur = base
        for part in (None, *parts):
            if part is not None:
                cur = os.path.join(cur, part)
            try:
                st = os.lstat(cur)
            except FileNotFoundError:
                return
            if stat.S_ISLNK(st.st_mode):
                raise OwnershipError(f"{cur} is a symbolic link: refusing to follow it")

    @staticmethod
    def _read_marker(path, what):
        try:
            st = os.lstat(path)
        except FileNotFoundError:
            raise OwnershipError(f"{what} ownership marker {path} is missing: this is not candidate state")
        if not stat.S_ISREG(st.st_mode):
            raise OwnershipError(f"{what} ownership marker {path} is not a regular file")
        try:
            with open(path) as f:
                m = json.load(f)
        except (OSError, ValueError) as e:
            raise OwnershipError(f"{what} ownership marker {path} is unreadable or forged: {e}")
        if not isinstance(m, dict) or m.get("runtime") != OWNER_RUNTIME or m.get("protocol") != OWNER_PROTOCOL:
            raise OwnershipError(f"{what} ownership marker {path} is not a {OWNER_RUNTIME} protocol {OWNER_PROTOCOL} marker")
        return m

    @staticmethod
    def _write_marker(path, data, exclusive):
        tmp = path + ".tmp"
        with open(tmp, "x") as f:
            json.dump(data, f, indent=2, sort_keys=True)
        if exclusive and os.path.lexists(path):
            os.remove(tmp)
            raise OwnershipError(f"{path} already exists")
        os.replace(tmp, path)

    def _require_root_owned(self):
        base = os.path.abspath(self.cfg["state_dir"])
        self._no_links(base)
        m = self._read_marker(os.path.join(base, ROOT_MARKER), "state-dir")
        if m.get("state_dir_real") != self._state_real():
            raise OwnershipError(f"the state-dir marker belongs to {m.get('state_dir_real')!r}, not {self._state_real()!r} (copied or stale state)")
        return m

    def _expected_slot_marker(self, slot):
        p = L.slot_paths(self.cfg["state_dir"], slot)
        return {"runtime": OWNER_RUNTIME, "protocol": OWNER_PROTOCOL, "slot": slot, "tree_marker": self._marker(slot),
                "state_dir_real": self._state_real(), "slot_dir_real": os.path.realpath(p["dir"]),
                "lower_real": os.path.realpath(self.cfg["lower"]), "image_sha256": self.cfg["image_sha256"]}

    def _require_owned(self, slot):
        """EVERY verb calls this first: root marker, slot marker (tree marker, state/slot dir, lower, image), no symlinked components."""
        self._require_root_owned()
        p = L.slot_paths(self.cfg["state_dir"], slot)
        base = os.path.abspath(self.cfg["state_dir"])
        self._no_links(base, f"slot{slot}")
        m = self._read_marker(os.path.join(p["dir"], SLOT_MARKER), f"slot {slot}")
        for k, v in self._expected_slot_marker(slot).items():
            if m.get(k) != v:
                raise OwnershipError(f"slot {slot} marker field {k} is {m.get(k)!r}, expected {v!r} (stale, copied or forged state)")
        for n in ("upper", "work", "merged", "images", "upper.snap", "slot.json", "criu-root"):
            self._no_links(p["dir"], n)
        return m

    def _check_sources(self, slot):
        m = self._require_owned(slot)
        now = source_sha256()
        if m.get("source_sha256") != now:
            raise ArmError(f"slot {slot}: the runtime sources changed since provisioning (recorded {m.get('source_sha256')}, now {now}): "
                           "a checkpoint made by other code is never restored; provision fresh state")
        return m

    def _slot(self, slot):
        self._require_owned(slot)
        return super()._slot(slot)

    def _preserve_logs(self, slot, label):
        try:
            self._require_owned(slot)
        except OwnershipError:
            return None
        return super()._preserve_logs(slot, label)

    def _init_state_root(self):
        """Create (or accept) the state root as candidate-owned. A non-empty directory without our marker is somebody else's state (the earlier
        custom rootfs-state with its saved dump, the OCI reference's state, anything) and is refused untouched."""
        base = os.path.abspath(self.cfg["state_dir"])
        if os.path.lexists(base):
            self._no_links(base)
            if os.path.lexists(os.path.join(base, ROOT_MARKER)):
                self._require_root_owned()
                return
            if os.listdir(base):
                raise OwnershipError(f"{base} is not empty and has no {ROOT_MARKER}: it holds other state (e.g. the earlier custom runtime's saved "
                                     "dump); use a NEW candidate state_dir")
        else:
            os.makedirs(base, mode=0o700)
        self._write_marker(os.path.join(base, ROOT_MARKER), {"runtime": OWNER_RUNTIME, "protocol": OWNER_PROTOCOL,
                                                           "state_dir_real": self._state_real(), "created_epoch": time.time()}, True)

    # ----------------------------------------------------------------------------------------- readback and identity
    def _proven_netns(self, pid):
        ns, host = self.ns_inode(pid), self.host_ns_inode()
        if ns is None or ns == host:
            raise ArmError(f"init {pid} is not in a separate network namespace (inode {ns}, host {host}): no namespace access")

    def _net_state(self, pid):
        """READ-ONLY readback from the host (`nsenter -t PID -n -- ip -j ...`), only after _proven_netns. Any failure is an error."""
        self._proven_netns(pid)
        outs = []
        for what in ("link", "addr", "route"):
            r = self.ex.run(Command("arm", ("nsenter", "-t", str(pid), "-n", "--", "ip", "-j", what, "show"), f"netns readback ({what})",
                                    timeout_s=30), 30)
            if r.rc != 0:
                raise ArmError(f"cannot read the init's {what} state with nsenter/ip: rc={r.rc} {(r.stderr or '')[-200:]!r}")
            try:
                outs.append(json.loads(r.stdout or "[]"))
            except ValueError:
                raise ArmError(f"ip -j {what} show printed non-JSON: {r.stdout[:120]!r}")
        return normalize_net(*outs)

    def _save_json(self, slot, name, data):
        path = os.path.join(L.slot_paths(self.cfg["state_dir"], slot)["dir"], name)
        with open(path, "x") as f:
            json.dump(data, f, indent=2, sort_keys=True)
        return path

    @staticmethod
    def mount_summary(text):
        return sorted([m["mountpoint"], m["fstype"], m["root"]] for m in cr.parse_mountinfo(text))

    def lock_census(self, pid, mount_text):
        """Locks on fds of the dumped tree, the outside-holder check against the host's /proc/locks, and the contract's verdict."""
        by_children, by_ns = sorted(self.walk(pid)), sorted(self.ns_members(pid))
        tree = sorted(set(by_children) | set(by_ns) | {pid})          # the PID namespace is the tree, whatever shape the ppid graph has
        locks = lk.census(tree, self.read_text, self.listdir, self.readlink)
        fstypes = {m["id"]: m["fstype"] for m in cr.parse_mountinfo(mount_text)}
        files = {f"{l['dev']}:{l['ino']}" for l in locks}
        outside = lk.outside_holders(self.read_text("/proc/locks"), tree, files) if locks else []
        return {"locks": locks, "outside": outside, "violations": lk.contract(locks, fstypes, outside), "tree_pids": tree,
                "by_children": by_children, "by_pid_ns": by_ns, "tree_shape_differs": set(by_children) | {pid} != set(by_ns)}

    @staticmethod
    def lock_summary(locks):
        return sorted([l["kind"], l["advisory"], l["type"], l["path"], l["start"], l["end"]] for l in locks)

    def _started(self, slot, pid, label, expect_net=None, expect_mounts=None, expect_locks=None):
        """After provision/restore and BEFORE the proxy: the init found by exact identity, its start time, its mount table (contract-clean at
        provision, equal to the dumped one after a restore) and its network namespace (loopback up at provision, equal to the dumped one)."""
        if self.th.find_init(self._marker(slot)) != pid:
            raise ArmError(f"init {pid} is not the slot's exact-identity init")
        started = self.start_time(pid)
        text = self.read_text(f"/proc/{pid}/mountinfo")
        summary = self.mount_summary(text)
        self._save_json(slot, f"mounts-{label}-{int(time.time() * 1000)}.json", summary)
        if expect_mounts is None:
            violations = cr.mount_contract(text)
            if violations:
                raise ArmError(f"the {label} init's mount table violates the contract: {violations}")
        elif summary != expect_mounts:
            raise ArmError(f"the mount table after {label} differs from the dumped one: before {expect_mounts}, after {summary}")
        if expect_locks is not None:
            # RECORDED, not gated: after the tasks run again a lock may legitimately be released or taken (an SQLite reader's shared lock is
            # transient). The fidelity gate is made where it is deterministic, before the tasks resume (see _lock_gates).
            census = self.lock_census(pid, text)
            self._save_json(slot, f"locks-post-resume-{label}-{int(time.time() * 1000)}.json",
                            {"locks": census["locks"], "tuples": lk.census_tuples(census["locks"]), "dumped": expect_locks, "gating": False,
                             "tree_shape_differs": census["tree_shape_differs"]})
        net = self._net_state(pid)
        self._save_json(slot, f"netstate-{label}-{int(time.time() * 1000)}.json", net)
        if expect_net is None:
            if not loopback_ok(net):
                raise ArmError(f"loopback is not up with 127.0.0.1/8 after {label}: {net['links'].get('lo')}")
        else:
            diffs = diff_net(expect_net, net)
            if diffs:
                raise ArmError(f"the network namespace after {label} differs from the checkpointed one: {'; '.join(diffs)[:600]}")
        return started

    # ----------------------------------------------------------------------------------------- verbs
    def provision(self, slot: int) -> dict:
        """Fresh state only: an owned state dir and a slot directory that does not exist yet (no reuse protocol on purpose)."""
        self._init_state_root()
        p = L.slot_paths(self.cfg["state_dir"], slot)
        if os.path.lexists(p["dir"]):
            raise OwnershipError(f"slot {slot} already exists at {p['dir']}: this runtime requires fresh state (cleanup, then a NEW state_dir or slot)")
        os.makedirs(p["dir"], mode=0o700)
        self._write_marker(os.path.join(p["dir"], SLOT_MARKER), {**self._expected_slot_marker(slot), "created_epoch": time.time(),
                                                               "source_sha256": source_sha256()}, True)
        self._require_owned(slot)
        rec = super().provision(slot)
        try:
            started = self._started(slot, rec["init_pid"], "provision")
            return self._record(slot, init_start_ticks=started, restore_generation=0)
        except BaseException:
            self._preserve_logs(slot, f"provision-gate-{int(time.time())}")
            self.sweep(slot)
            raise

    def snapshot(self, slot: int) -> dict:
        """Gate, dump (the tree stops), copy the upper layer, restore: a process+disk snapshot and the first restore."""
        st = self._slot(slot)
        self._check_sources(slot)
        p = L.slot_paths(self.cfg["state_dir"], slot)
        if os.path.exists(os.path.join(p["dir"], "upper.snap")):
            raise ArmError("upper.snap already exists: a slot has one snapshot; run cleanup first")
        if os.listdir(p["images"]):
            raise ArmError("images dir is not empty: refusing to overwrite an existing dump")
        pid = self._wait_init(slot)
        if pid != st.get("init_pid") or self.start_time(pid) != st.get("init_start_ticks"):
            raise ArmError(f"init {pid} is not the process recorded for slot {slot} (pid/start time differ): refusing to dump it")
        violations = cr.mount_contract(self.read_text(f"/proc/{pid}/mountinfo"))
        if violations:
            raise ArmError(f"refusing to dump: the init's mount table needs external mounts this restore does not provide: {violations}")
        mount_text = self.read_text(f"/proc/{pid}/mountinfo")
        pre = self._net_state(pid)
        if not loopback_ok(pre):
            raise ArmError(f"refusing to dump: loopback is not up with 127.0.0.1/8: {pre['links'].get('lo')}")
        self._save_json(slot, "netstate-pre-dump.json", pre)
        self._save_json(slot, "mounts-pre-dump.json", self.mount_summary(mount_text))
        census = self.lock_census(pid, mount_text)
        self._save_json(slot, "locks-census.json", {**census, "file_locks_flag": bool(census["locks"]) and not census["violations"]})
        if census["violations"]:
            raise ArmError(f"refusing to dump: the file locks held by the tree do not satisfy criu(8)'s enclosure condition: {census['violations']}")
        file_locks = bool(census["locks"])               # no lock => no flag: CRIU keeps failing closed on any lock it finds
        self._record(slot, file_locks=file_locks)
        self._run(L.dump_argv(pid, p["images"], os.path.join(p["dir"], "dump.log"), file_locks=file_locks,
                              externals=cr.dump_externals(mount_text)), "criu dump (stops the tree)", 900)
        if not os.listdir(p["images"]):
            raise ArmError("criu dump wrote no images")
        manifest = net_image_manifest(p["images"])
        self._save_json(slot, "images-manifest.json", manifest)
        if manifest["missing_required"]:
            raise ArmError(f"the checkpoint lacks the network images {manifest['missing_required']}: the private network namespace was NOT "
                           "checkpointed, so a restore could not reproduce it (nothing restored)")
        self._record_dumped_locks(slot, p, file_locks, census)
        stale = rt.sweep_marker(self.ph, self._marker(slot))
        if not stale["ok"]:
            raise ArmError(f"cannot stop the proxy before restoring: {stale}")
        self._run(("cp", "-a", "--reflink=auto", p["upper"], os.path.join(p["dir"], "upper.snap")), "copy upper layer", 1800)
        self._record(slot, state="dumped")
        return self._restore(slot, "post-snapshot")

    def _record_dumped_locks(self, slot, p, file_locks, census):
        """CRIU's own record of what it dumped is the authority for the restore gate, not the pre-dump census (a transient lock can be released
        between the census and the moment CRIU seizes the tasks): the `LOCK flag:` lines of dump.log, cross-checked against filelocks.img."""
        with open(os.path.join(p["dir"], "dump.log")) as f:
            dumped = lk.dumped_from_log(f.read())
        img = os.path.join(p["images"], "filelocks.img")
        img_sha = hashlib.sha256(open(img, "rb").read()).hexdigest() if os.path.isfile(img) else None
        if dumped and (img_sha is None or os.path.getsize(img) == 0):
            raise ArmError("dump.log recorded file locks but filelocks.img is missing or empty: the checkpoint is not usable")
        if dumped and not file_locks:
            raise ArmError("dump.log recorded file locks although --file-locks was not used")
        self._save_json(slot, "locks-dumped.json", {"dumped": dumped, "filelocks_img_sha256": img_sha, "census_tuples": lk.census_tuples(census["locks"]),
                                                    "census_vs_dump": "equal" if [list(t) for t in lk.shape_of_dump(dumped)] == lk.shape_of_census(census["locks"]) else "differ (a transient lock released before the dump, or the reverse)"})

    def _lock_gates(self, slot, p, gen, hook_dir, restore_log):
        """Deterministic restore-lock fidelity: (1) what the restore APPLIED (restore log) equals what was DUMPED (dump log + filelocks.img hash);
        (2) the census taken by CRIU's own pre-resume action, while the restored tasks are still stopped, equals the dumped set. Both before any
        task ran, so a legitimate release by a resumed task cannot be mistaken for a lost lock, and a really lost lock cannot hide behind it."""
        with open(os.path.join(p["dir"], "locks-dumped.json")) as f:
            rec = json.load(f)
        img = os.path.join(p["images"], "filelocks.img")
        now = hashlib.sha256(open(img, "rb").read()).hexdigest() if os.path.isfile(img) else None
        if now != rec["filelocks_img_sha256"]:
            raise ArmError("filelocks.img changed between the dump and the restore")
        dumped = rec["dumped"]
        with open(restore_log) as f:
            applied = lk.applied_from_log(f.read())
        if applied != sorted(dumped, key=str):
            raise ArmError(f"the restore applied file locks {applied} but the dump recorded {dumped}")
        path = os.path.join(hook_dir, "locks-pre-resume.json")
        try:
            with open(path) as f:
                hook = json.load(f)
        except (OSError, ValueError):
            raise ArmError(f"CRIU's pre-resume action left no readable census at {path}: the restored locks cannot be verified before the tasks run")
        if hook.get("error"):
            raise ArmError(f"the pre-resume census failed: {hook['error']}")
        want = [list(t) for t in lk.shape_of_dump(dumped)]
        if hook["shape"] != want:
            raise ArmError(f"before the tasks resumed the restored tree holds locks {hook['shape']} but the dump recorded {want}; the host's /proc/locks "
                           f"shows for the tree {hook.get('host_proc_locks_of_tree')}; locks the holder also maps: "
                           f"{[(l['path'], l['mapped_by_holder']) for l in hook.get('locks', [])]}")
        return {"dumped": dumped, "applied": applied, "pre_resume": hook["tuples"], "tree_shape_differs": hook.get("tree_pids") != sorted(set(hook.get("by_pid_ns", [])) | set(hook.get("by_children", [])))}

    def _write_hook(self, slot, p, gen):
        """The action script CRIU runs (criu(8) --action-script): a read-only census at pre-resume/post-resume. Generated per restore, owned by root."""
        hook_dir = os.path.join(p["dir"], f"lock-hook-{gen}")
        os.mkdir(hook_dir, 0o700)
        script = os.path.join(hook_dir, "hook.sh")
        pkg = os.path.dirname(os.path.abspath(__file__))
        fd = os.open(script, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o700)
        with os.fdopen(fd, "w") as f:
            f.write("#!/bin/sh\n# generated by sharedos_rootfs.candidate: read-only lock census at CRIU's pre-resume and post-resume actions\n"
                    f"exec {shlex.quote(sys.executable)} -I {shlex.quote(os.path.join(pkg, 'locks.py'))} --hook {shlex.quote(hook_dir)}\n")
        return script, hook_dir

    def _restore(self, slot, label):
        self._require_owned(slot)
        p = L.slot_paths(self.cfg["state_dir"], slot)
        croot = os.path.join(p["dir"], "criu-root")
        if os.path.lexists(croot):
            if os.path.islink(croot) or not os.path.isdir(croot) or os.listdir(croot) or self.ex.run(Command("arm", ("findmnt", "-n", croot), "mounted?"), 30).rc == 0:
                raise ArmError(f"{croot} exists and is not an empty, unmounted directory: refusing to use it as the CRIU root")
            os.rmdir(croot)
        pre_path = os.path.join(p["dir"], "netstate-pre-dump.json")
        try:
            with open(pre_path) as f:
                expect = json.load(f)
        except (OSError, ValueError):
            raise ArmError(f"{pre_path} is missing or unreadable: there is no checkpointed network state to verify the restore against")
        try:
            with open(os.path.join(p["dir"], "mounts-pre-dump.json")) as f:
                expect_mounts = json.load(f)
        except (OSError, ValueError):
            raise ArmError("mounts-pre-dump.json is missing or unreadable: there is no dumped mount table to verify the restore against")
        try:
            with open(os.path.join(p["dir"], "locks-dumped.json")) as f:
                expect_locks = json.load(f)["dumped"]
        except (OSError, ValueError, KeyError):
            raise ArmError("locks-dumped.json is missing or unreadable: there is no dumped lock set to verify the restore against")
        file_locks = bool(self._slot(slot).get("file_locks"))
        os.mkdir(croot, 0o700)
        pidfile = os.path.join(p["dir"], "restored.pid")
        if os.path.exists(pidfile):
            os.remove(pidfile)
        gen = self._slot(slot).get("restore_generation", 0) + 1
        self._record(slot, restore_generation=gen)
        log = os.path.join(p["dir"], f"restore-{label}-{int(time.time())}.log")
        hook_script, hook_dir = self._write_hook(slot, p, gen)
        try:
            try:
                lower, userns = self._restore_lower(p)
                argv = cr.restore_argv(self._marker(slot), gen, self.cfg["vcpus"], self.cfg["memory_mib"], p["images"], pidfile, log,
                                       lower, p["upper"], p["work"], croot, file_locks=file_locks, action_script=hook_script,
                                       userxattr=userns,
                                       externals=cr.restore_externals(expect_mounts or [], self.cfg.get("procview_dir") or "/run/drums/procview"))
            except cr.ContractError as e:
                raise ArmError(str(e))
            self._run(argv, "criu restore (scope + private mount namespace, overlay as --root)", 600)
            pid = self.th.read_pid(pidfile)
            if pid is None:
                raise ArmError("criu restore wrote no pidfile")
            self._save_json(slot, f"locks-gates-{label}-{gen}.json", self._lock_gates(slot, p, gen, hook_dir, log))
            self._verify_cgroup(slot, pid)
            started = self._started(slot, pid, label, expect_net=expect, expect_mounts=expect_mounts, expect_locks=expect_locks)
            self._start_proxy(slot, pid)
            self._wait_server(slot)
            if self.start_time(pid) != started or self.th.find_init(self._marker(slot)) != pid:
                raise ArmError(f"init {pid} is not the same process after the readiness checks")
        except BaseException:
            self._preserve_logs(slot, f"restore-{label}-{int(time.time())}")
            self.sweep(slot)
            raise
        finally:
            try:
                os.rmdir(croot)
            except OSError:
                pass
        return self._record(slot, init_pid=pid, init_start_ticks=started, state="running")

    def _restore_lower(self, p) -> str:
        """The lower layer of the restore root: the idmapped view the checkpointed slot ran on when its plan (copied with every
        checkpoint) says it ran in its own user namespace, else the image itself."""
        try:
            with open(p["plan"]) as f:
                userns = json.load(f).get("userns")
        except (OSError, ValueError):
            raise ArmError(f"{p['plan']} is missing or unreadable: the restore cannot tell which lower layer the checkpoint ran on")
        if not userns:
            return self.cfg["lower"], False
        return self.userns.idmapped_lower(self.cfg["lower"], int(userns["base"])), True

    def restore(self, slot: int) -> dict:
        self._slot(slot)
        self._check_sources(slot)
        return super().restore(slot)

    reset = restore

    def endpoint(self, slot: int) -> dict:
        self._require_owned(slot)
        return super().endpoint(slot)

    def cleanup(self) -> dict:
        """Only owned state is touched: the root must carry our marker; each slot is validated individually and a foreign/forged/stale one is
        reported and left exactly as it is. Files are removed only after every process of the slot is proven stopped (pidfd-pinned sweep)."""
        base = os.path.abspath(self.cfg["state_dir"])
        out = {"ok": True, "slots": {}}
        if not os.path.lexists(base):
            return out
        self._require_root_owned()
        for name in sorted(os.listdir(base)):
            if not (name.startswith("slot") and name[4:].isdigit()):
                continue
            slot = int(name[4:])
            p = L.slot_paths(self.cfg["state_dir"], slot)
            rec = {"sweep_ok": False, "removed": []}
            out["slots"][name] = rec
            try:
                self._require_owned(slot)
            except OwnershipError as e:
                rec["refused"] = str(e)
                out["ok"] = False
                continue
            s = self.sweep(slot)
            if not s["ok"]:
                rec["evidence"] = s
                out["ok"] = False
                continue
            rec["sweep_ok"] = True
            self._preserve_logs(slot, "at-cleanup")
            for d in armmod.HEAVY + ("criu-root",):
                path = os.path.join(p["dir"], d)
                if not os.path.lexists(path):
                    continue
                if os.path.islink(path) or not os.path.isdir(path) or not os.path.realpath(path).startswith(os.path.realpath(p["dir"]) + os.sep):
                    rec["sweep_ok"], rec["why"] = False, f"{path} is a link or outside the slot: not deleted"
                    continue
                if self.ex.run(Command("arm", ("findmnt", "-n", path), "mounted?"), 30).rc == 0:
                    rec["sweep_ok"], rec["why"] = False, f"{path} is still a mount point: not deleted"
                    continue
                shutil.rmtree(path)
                rec["removed"].append(d)
            out["ok"] = out["ok"] and rec["sweep_ok"]
        return out

    def environment(self) -> str:
        missing = [t for t in TOOLS if not self.th.which(t)]
        if missing:
            raise ArmError(f"missing tools: {missing}")
        text = super().environment()
        c = self.ex.run(Command("arm", ("criu", "--version"), "criu version"), 30)
        if c.rc != 0 or "Version: 4.3" not in (c.stdout or c.stderr):
            raise ArmError(f"criu --version is not the pinned 4.3: {(c.stdout or c.stderr).strip()[:80]!r}")
        blocking = criu_config_files(os.environ.get("HOME", ""), os.environ)
        if blocking:
            raise ArmError(f"CRIU configuration files would silently change checkpoint/restore behaviour: {blocking}")
        return text + f"\ncriu: 4.3 (pinned version string)\ncriu binary: {self._criu_binary_identity()}\ncriu config files: none"

    def _criu_binary_identity(self) -> str:
        """The version string alone cannot tell a patched 4.3 from the stock one, so the receipt names the exact binary (resolved path + sha256)."""
        path = self.th.which("criu")
        try:
            with open(path, "rb") as f:
                return f"{path} sha256={hashlib.sha256(f.read()).hexdigest()}"
        except (OSError, TypeError) as e:
            return f"{path!r} unreadable ({type(e).__name__}): identity NOT recorded"

    def describe(self) -> dict:
        d = super().describe()
        d.update({"arm": "sharedos-rootfs-candidate",
                  "label": "Drums CANDIDATE (custom runtime, CRIU direct); NOT a Drums result until a live gate passes; the OCI reference is the separate control",
                  "criu_restore_argv_template": list(cr.restore_argv("MARKER", 1, self.cfg["vcpus"], self.cfg["memory_mib"], "IMAGES", "PIDFILE",
                                                                    "LOG", "LOWER", "UPPER", "WORK", "CRIU_ROOT")),
                  "restore_root_contract": cr.__doc__.strip().splitlines()[0:1] + ["see criu_root.py / CANDIDATE.md"],
                  "network": "CRIU checkpoints and restores the slot's own private network namespace (no empty-ns, no external, no holder)",
                  "source_sha256": source_sha256()})
        return d


def main(argv: list, executor=None, tree_host=None, proxy_host=None) -> int:
    from drumsbench.executor import SubprocessExecutor
    if not argv:
        raise SystemExit(__doc__)
    verb, rest = argv[0], argv[1:]
    try:
        cfg = armmod.load_config(os.environ.get("SHAREDOS_ROOTFS_CONFIG", ""))
        arm = CandidateArm(cfg, executor or SubprocessExecutor(), tree_host or rt.Host(module=L.TREE_MODULE),
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
