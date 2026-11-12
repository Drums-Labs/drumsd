"""The Drums CANDIDATE arm (sharedos_rootfs/candidate.py, criu_root.py) with FAKE criu/systemd/nsenter/hosts. These tests prove the restore
argv, ownership of every verb, gates and cleanup logic. They are NOT evidence that CRIU restores the real tree: that is the operator's live gate."""

import io
import json
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(__file__))
from sharedos_rootfs import harden as H  # noqa: E402
from rootfs_helpers import FakeHost, FakeUserns, Res, ScriptExec, World, arm_exec, fake_cgroup_reader, make_rootfs  # noqa: E402

from osworld_smoke import pins  # noqa: E402
from sharedos_rootfs import arm as armmod  # noqa: E402
from sharedos_rootfs import candidate as cand  # noqa: E402
from sharedos_rootfs import criu_root as cr  # noqa: E402
from sharedos_rootfs import launch as L  # noqa: E402
from sharedos_rootfs import rootfs_inspect as ri  # noqa: E402
from sharedos_rootfs.arm import ArmError  # noqa: E402

GOOD_MOUNTINFO = """\
21 1 0:30 / / rw,relatime - overlay overlay rw,lowerdir=/l,upperdir=/u,workdir=/w
22 21 0:5 / /proc rw,nosuid - proc proc rw
23 21 0:6 / /sys ro,nosuid - sysfs sysfs ro
24 21 0:7 / /dev rw,nosuid - tmpfs tmpfs rw,size=65536k,mode=755
25 24 0:8 / /dev/pts rw,nosuid - devpts devpts rw,gid=5,mode=620
26 24 0:9 / /dev/shm rw,nosuid - tmpfs tmpfs rw
27 21 0:10 / /tmp rw,nosuid - tmpfs tmpfs rw
28 21 0:11 / /run rw,nosuid - tmpfs tmpfs rw
40 24 0:2 /null /dev/null rw,nosuid - devtmpfs udev rw
41 24 0:2 /zero /dev/zero rw,nosuid - devtmpfs udev rw
42 24 0:2 /full /dev/full rw,nosuid - devtmpfs udev rw
43 24 0:2 /random /dev/random rw,nosuid - devtmpfs udev rw
44 24 0:2 /urandom /dev/urandom rw,nosuid - devtmpfs udev rw
45 24 0:2 /tty /dev/tty rw,nosuid - devtmpfs udev rw
"""
# (the six /dev rows: the host device binds of a slot in its own user namespace, harden.DEV_BIND_NODES)
DEV_ROWS = [[f"/dev/{n}", "devtmpfs", f"/{n}"] for n in ("null", "zero", "full", "random", "urandom", "tty")]
HOST_NS = 1111


def tmp(tc):
    d = tempfile.mkdtemp()
    tc.addCleanup(shutil.rmtree, d, True)
    return d


class CWorld(World):
    def __init__(self, **flags):
        super().__init__(**flags)
        self.mountinfo = {}      # pid -> text
        self.ns = {}             # pid -> netns inode
        self.restore_argvs = []
        self.nsenter_calls = []
        self.criu_dumps = 0
        self.dump_argvs = []
        self.lock_fds = []             # [(fd, "lock:" fdinfo line or None, link, mnt_id)] seen on every task of the (single-process) fake tree
        self.restored_lock_fds = None  # what the restored tree shows AFTER resume (None = the same)
        self.proc_locks = ""
        self.applied_override = None   # lines the fake restore logs as applied (None = exactly what was dumped)
        self.hook_shape = None         # the pre-resume census the fake hook writes (None = what was dumped)
        self.hook_mode = "ok"          # ok | missing | error
        self.filelocks_changed = False
        self.dumped_lines = []


def cand_exec(world):
    base = arm_exec(world).fn

    def fn(argv):
        argv = list(argv)
        name = argv[0]
        if name == "nsenter":
            world.nsenter_calls.append(argv)
            if world.f.get("net_read_fails"):
                return Res(1, "", "nsenter: denied")
            pid, what = int(argv[argv.index("-t") + 1]), argv[-2]
            down = pid in world.f.get("restored", set()) and world.f.get("restored_lo_down")
            if what == "link":
                return Res(0, json.dumps([{"ifindex": 1, "ifname": "lo", "flags": ["LOOPBACK"] if down else ["LOOPBACK", "UP", "LOWER_UP"], "mtu": 65536}]))
            if what == "addr":
                info = [] if down else [{"family": "inet", "local": "127.0.0.1", "prefixlen": 8, "scope": "host"}]
                return Res(0, json.dumps([{"ifindex": 1, "ifname": "lo", "addr_info": info}]))
            return Res(0, "[]")
        if name == "systemd-run" and "--mount" in argv and "restore" in argv:         # the candidate's restore: scope + private mntns + overlay root
            world.restore_argvs.append(argv)
            if world.f.get("restore_fails"):
                return Res(1, "", "Error: restore failed")
            inner = argv[argv.index("criu"):]
            world.events.append(("scoped_restore", argv[argv.index("--unit") + 1], argv))
            if world.filelocks_changed:
                with open(os.path.join(argv[argv.index("-D") + 1], "filelocks.img"), "w") as f:
                    f.write("tampered")
            world.pending_unit = argv[argv.index("--unit") + 1]
            try:
                r = base(inner)
            finally:
                world.pending_unit = None
            pid = [p for p in world.trees][-1]
            world.mountinfo[pid] = world.f.get("restored_mountinfo", GOOD_MOUNTINFO)
            world.ns[pid] = world.f.get("restored_ns", 3000 + pid)
            world.f.setdefault("restored", set()).add(pid)
            # what CRIU would have logged: the applied locks, and the census its pre-resume/post-resume action script would have written
            from sharedos_rootfs import locks as lkm
            applied = world.applied_override if world.applied_override is not None else world.dumped_lines
            with open(argv[argv.index("-o") + 1], "a") as f:
                for line in applied:
                    f.write(line.replace("LOCK flag: ", "(posix)flag: ").replace(",type: ", ", type: ").replace(",pid: ", ", pid: ").replace(",fd: ", ", fd: ")
                            .replace(",start: ", ", start: ").replace(",len: ", ", len: "))
                f.write("(00.9) Restore finished successfully. Tasks resumed.\n")
            script = argv[argv.index("--action-script") + 1]
            hook_dir = os.path.dirname(script)
            if world.hook_mode != "missing":
                shape = world.hook_shape if world.hook_shape is not None else [list(t) for t in lkm.shape_of_dump(lkm.dumped_from_log("".join(world.dumped_lines)))]
                rec = {"action": "pre-resume", "error": "boom"} if world.hook_mode == "error" else {"action": "pre-resume", "shape": shape, "tuples": [],
                                                                                                      "tree_pids": [pid], "by_pid_ns": [pid], "by_children": []}
                with open(os.path.join(hook_dir, "locks-pre-resume.json"), "w") as f:
                    json.dump(rec, f)
            return r
        if name == "criu" and argv[1] == "dump":
            world.dump_argvs.append(argv)
            r = base(argv)
            if r is None:
                images = argv[argv.index("-D") + 1]
                for k in world.f.get("net_images", ("netns-10.img", "netdev-10.img", "ifaddr-10.img")):
                    open(os.path.join(images, k), "w").close()
                world.criu_dumps += 1
                from sharedos_rootfs import locks as lkm
                lines = []
                for fd, lock, link, mnt in world.lock_fds:
                    if lock and "--file-locks" in argv:
                        e = lkm.parse_lock(lock)
                        ln = 0 if e["end"] == "EOF" else int(e["end"]) + 1 - e["start"]
                        lines.append(f"(00.5) LOCK flag: {lkm.KIND_CODE[e['kind']]},type: {lkm.TYPE_CODE[e['type']]},pid: 52,fd: {fd},start: {e['start']:8x},len: {ln:8x}\n")
                world.dumped_lines = lines
                with open(argv[argv.index("-o") + 1], "a") as f:
                    f.write("(00.1) dump started\n" + "".join(lines))
                if lines:
                    with open(os.path.join(images, "filelocks.img"), "w") as f:
                        f.write("locks")
            return r
        if name == "systemd-run" and "--marker" in argv:
            before = set(world.trees)
            r = base(argv)
            for pid in set(world.trees) - before:
                world.mountinfo[pid] = world.f.get("mountinfo", GOOD_MOUNTINFO)
                world.ns[pid] = world.f.get("tree_ns", 2000 + pid)
            return r
        if name == "criu" and argv[1:] == ["--version"]:
            return Res(0, "Version: 4.3.0\n")
        return base(argv)
    return ScriptExec(fn)


class CEnv:
    def __init__(self, tc):
        self.d = tmp(tc)
        self.lower = make_rootfs(os.path.join(self.d, "lower"))
        insp = ri.inspect_rootfs(self.lower, pins.IMAGE_QCOW_SHA256)
        self.insp_path = os.path.join(self.d, "inspection.json")
        with open(self.insp_path, "w") as f:
            json.dump(insp, f)
        self.state = os.path.join(self.d, "state")
        self.cfg_path = os.path.join(self.d, "config.json")
        self.cfg_dict = {"state_dir": self.state, "lower": self.lower, "inspection": self.insp_path, "image_sha256": pins.IMAGE_QCOW_SHA256,
                         "vcpus": 4, "memory_mib": 4096, "screen": "1920x1080x24", "profile": "gnome_x11", "service_base_port": 26000,
                         "guest_service_port": 5000}

    def arm(self, world=None, state=None, **over):
        world = world or CWorld()
        d = dict(self.cfg_dict, **over)
        if state:
            d["state_dir"] = state
        with open(self.cfg_path, "w") as f:
            json.dump(d, f)
        ex = cand_exec(world)
        cfg = armmod.load_config(self.cfg_path)

        def http(url, timeout=5.0):
            if world.f["server_dead"] or not (world.trees and world.proxies):
                raise OSError("connection refused")
            return "Linux"
        def fds_of(pid):
            return world.restored_lock_fds if (pid in world.f.get("restored", set()) and world.restored_lock_fds is not None) else world.lock_fds

        def read_text(path):
            if path == "/proc/locks":
                return world.proc_locks
            parts = path.split("/")
            if len(parts) > 4 and parts[3] == "fdinfo":
                fd = int(parts[4])
                for f, lock, link, mnt in fds_of(int(parts[2])):
                    if f == fd:
                        return f"pos:\t0\nflags:\t0100002\nmnt_id:\t{mnt}\n" + (f"lock:\t{lock}\n" if lock else "")
                raise OSError("no such fd")
            return world.mountinfo[int(parts[2])]

        def listdir(path):
            return [str(f) for f, *_ in fds_of(int(path.split("/")[2]))]

        def readlink(path):
            parts = path.split("/")
            return next(link for f, lock, link, mnt in fds_of(int(parts[2])) if f == int(parts[4]))
        a = cand.CandidateArm(cfg, ex, FakeHost(world, "tree", tools=cand.TOOLS), FakeHost(world, "proxy"), sleep=lambda s: None, http=http, wait_s=0.05,
                              controllers_path=os.path.join(self.d, "ctl"), cgroup_reader=fake_cgroup_reader(world),
                              start_time=lambda pid: f"{pid}00",
                              read_text=read_text, listdir=listdir, readlink=readlink, walk=lambda pid: [], ns_members=lambda pid: [pid],
                              ns_inode=lambda pid: world.ns.get(pid), host_ns_inode=lambda: HOST_NS, userns_host=FakeUserns())
        return a, world, ex


class RestoreContractTests(unittest.TestCase):
    def test_restore_argv_is_scope_private_mntns_overlay_root(self):
        argv = cr.restore_argv("m-0", 2, 4, 4096, "/s/images", "/s/pid", "/s/log", "/img", "/s/upper", "/s/work", "/s/criu-root",
                               externals=cr.restore_externals(DEV_ROWS, "/run/drums/procview"))
        self.assertEqual(argv[:2], ("systemd-run", "--scope"))
        i = argv.index("unshare")
        self.assertEqual(argv[i:i + 5], ("unshare", "--mount", "--propagation", "private", "--"))
        self.assertEqual(argv[argv.index("sh") + 1], "-c")
        script = argv[argv.index("-c") + 1]
        self.assertEqual(script, 'mount -t overlay overlay -o "$1" "$2" || exit 97; shift 2; exec "$@"')
        ci = argv.index("-c")
        self.assertEqual(argv[ci + 2:ci + 5], ("sh", "lowerdir=/img,upperdir=/s/upper,workdir=/s/work", "/s/criu-root"))
        c = argv.index("criu")
        self.assertEqual(argv[c + 1], "restore")
        self.assertEqual(argv[argv.index("--root") + 1], "/s/criu-root")
        for forbidden in ("--mntns-compat-mode", "--empty-ns", "--ext-mount-map", "--inherit-fd", "--skip-mnt", "--enable-fs", "--force-irmap",
                          "--tcp-established", "--join-ns"):
            self.assertNotIn(forbidden, argv)
        # the only external mounts are the six host device nodes a slot in its own user namespace binds (harden.py), each by name
        ext = [argv[i + 1] for i, x in enumerate(argv) if x == "--external"]
        self.assertEqual(ext, [f"mnt[drums-dev-{n}]:/dev/{n}" for n in ("null", "zero", "full", "random", "urandom", "tty")])
        self.assertIn("--manage-cgroups=ignore", argv)
        # a slot with no device binds (not in its own user namespace) and no view has no external mount at all
        self.assertNotIn("--external", cr.restore_argv("m-0", 2, 4, 4096, "/s/images", "/s/pid", "/s/log", "/img", "/s/upper", "/s/work",
                                                       "/s/criu-root", externals=cr.restore_externals([], "/run/drums/procview")))
        self.assertIn(f"drumsbench-m-0-r2.scope", argv)

    def test_paths_that_cannot_be_overlay_options_are_refused(self):
        for bad in ("/a,b", "/a:b", "/a b"):
            with self.assertRaises(cr.ContractError):
                cr.restore_argv("m", 1, 4, 4096, "i", "p", "l", bad, "/u", "/w", "/r")

    def test_values_never_enter_the_script_text(self):
        argv = cr.restore_argv("m", 1, 4, 4096, "i", "p", "l", "/lo$(x)", "/u", "/w", "/r$(x)")
        self.assertNotIn("$(", argv[argv.index("-c") + 1])
        self.assertIn("/lo$(x)", argv[argv.index("-c") + 3])

    def test_the_shell_wrapper_mounts_first_then_execs_criu_and_fails_closed(self):
        import subprocess
        d = tmp(self)
        log = os.path.join(d, "log")
        for name, body in (("mount", 'echo "mount $@" >> "$LOG"; [ -z "$MOUNT_FAIL" ]'), ("criu", 'echo "criu $@" >> "$LOG"')):
            with open(os.path.join(d, name), "w") as f:
                f.write("#!/bin/sh\n" + body + "\n")
            os.chmod(os.path.join(d, name), 0o755)
        argv = cr.restore_argv("m", 1, 4, 4096, "/i", "/p", "/l", "/img", "/u", "/w", "/croot")
        sh = list(argv[argv.index("sh"):])                       # the part after `unshare ... --`: sh -c SCRIPT sh OPTS ROOT criu ...
        env = {"PATH": d + ":/usr/bin:/bin", "LOG": log}
        r = subprocess.run(sh, env=env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        lines = open(log).read().splitlines()
        self.assertEqual(lines[0], "mount -t overlay overlay -o lowerdir=/img,upperdir=/u,workdir=/w /croot")
        self.assertTrue(lines[1].startswith("criu restore -D /i --root /croot --restore-detached"))
        os.remove(log)
        r = subprocess.run(sh, env={**env, "MOUNT_FAIL": "1"}, capture_output=True, text=True)
        self.assertEqual(r.returncode, 97)
        self.assertEqual(open(log).read().count("criu"), 0)       # criu never runs without its root

    def test_mount_contract(self):
        self.assertEqual(cr.mount_contract(GOOD_MOUNTINFO), [])
        for text, why in ((GOOD_MOUNTINFO + "30 21 8:1 /data /data rw - ext4 /dev/sda1 rw\n", "ext4"),
                          (GOOD_MOUNTINFO + "30 24 0:12 /null /dev/null rw - tmpfs tmpfs rw\n", "bind of a host path"),
                          (GOOD_MOUNTINFO + "30 21 0:13 / /sys/kernel/security rw - securityfs securityfs rw\n", "securityfs"),
                          (GOOD_MOUNTINFO.replace("overlay overlay", "ext4 /dev/x", 1), "overlay"),
                          (GOOD_MOUNTINFO + "99 1 0:1 / / rw - overlay overlay rw\n", "exactly one")):
            bad = cr.mount_contract(text)
            self.assertTrue(any(why in b for b in bad), (why, bad))
        with self.assertRaises(cr.ContractError):
            cr.parse_mountinfo("garbage")


class NetHelpersTests(unittest.TestCase):
    def test_parity_with_the_reference_helpers_on_the_same_input(self):
        import importlib.util
        if importlib.util.find_spec("oci_reference") is None:
            import pytest
            pytest.skip("oci_reference is the benchmark control arm, not shipped with drumsd; see drums-research-history archive/drums-lab-f17644b")
        from oci_reference import driver as ref
        links = [{"ifindex": 1, "ifname": "lo", "mtu": 65536, "flags": ["LOOPBACK", "UP", "LOWER_UP"], "operstate": "UNKNOWN"}]
        addrs = [{"ifname": "lo", "addr_info": [{"family": "inet", "local": "127.0.0.1", "prefixlen": 8, "scope": "host"}]}]
        a, b = cand.normalize_net(links, addrs, []), ref.normalize_net(links, addrs, [])
        self.assertEqual(a, b)
        self.assertTrue(cand.loopback_ok(a) and ref.loopback_ok(b))
        down = cand.normalize_net([{"ifindex": 1, "ifname": "lo", "mtu": 65536, "flags": ["LOOPBACK"]}], [], [])
        self.assertEqual(cand.diff_net(a, down), ref.diff_net(a, down))
        self.assertFalse(cand.loopback_ok(down))

    def test_net_image_manifest(self):
        d = tmp(self)
        for n in ("core-1.img", "netns-10.img", "netdev-10.img"):
            open(os.path.join(d, n), "w").close()
        m = cand.net_image_manifest(d)
        self.assertEqual(m["missing_required"], ["ifaddr"])


class CandidateArmTests(unittest.TestCase):
    def provisioned(self, **flags):
        e = CEnv(self)
        a, w, ex = e.arm(CWorld(**flags))
        a.provision(0)
        return e, a, w, ex, L.slot_paths(a.cfg["state_dir"], 0)

    def test_provision_is_fresh_state_with_markers_and_a_source_hash(self):
        e, a, w, ex, p = self.provisioned()
        root = json.load(open(os.path.join(a.cfg["state_dir"], cand.ROOT_MARKER)))
        slot = json.load(open(os.path.join(p["dir"], cand.SLOT_MARKER)))
        self.assertEqual(root["runtime"], cand.OWNER_RUNTIME)
        self.assertEqual(slot["source_sha256"], cand.source_sha256())
        self.assertEqual(slot["tree_marker"], a._marker(0))
        self.assertTrue(a._slot(0)["init_start_ticks"])
        with self.assertRaises(cand.OwnershipError):                # a used slot is never re-provisioned in place
            a.provision(0)

    def test_foreign_state_dir_is_refused_untouched(self):
        e = CEnv(self)
        os.makedirs(e.state)
        open(os.path.join(e.state, "slot0.dump"), "w").close()      # e.g. the earlier custom runtime's saved state
        a, w, ex = e.arm()
        before = sorted(os.listdir(e.state))
        with self.assertRaises(cand.OwnershipError):
            a.provision(0)
        self.assertEqual(sorted(os.listdir(e.state)), before)
        self.assertEqual([x for x in w.events if x[0] == "launch"], [])

    def test_snapshot_restore_reset_use_the_root_contract_and_keep_the_network(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        self.assertEqual(len(w.restore_argvs), 1)
        argv = w.restore_argvs[0]
        self.assertEqual(argv[argv.index("--root") + 1], os.path.join(p["dir"], "criu-root"))
        self.assertNotIn("--file-locks", argv)                       # no lock held: the flag is absent and CRIU keeps failing closed
        self.assertNotIn("--file-locks", w.dump_argvs[0])
        self.assertFalse(os.path.exists(os.path.join(p["dir"], "criu-root")))      # no leftover mountpoint directory
        a.restore(0)
        a.reset(0)
        self.assertEqual(len(w.restore_argvs), 3)
        self.assertEqual(a._slot(0)["restore_generation"], 3)
        # nothing retained for the network: no holder, no empty-ns/external in ANY criu command
        flat = [x for c in ex.calls for x in c]
        for forbidden in ("--empty-ns", "--inherit-fd", "--mntns-compat-mode"):
            self.assertNotIn(forbidden, flat)
        ext = {flat[i + 1] for i, x in enumerate(flat) if x == "--external"}      # only the named device binds, never the network
        self.assertEqual(ext, set(H.criu_external_dump_args()[1::2]) | set(H.criu_external_restore_args()[1::2]))

    def test_dump_is_refused_for_external_mounts_before_criu_runs(self):
        e, a, w, ex, p = self.provisioned()
        pid = a._slot(0)["init_pid"]
        w.mountinfo[pid] = GOOD_MOUNTINFO + "30 24 0:12 /null /dev/null rw - tmpfs tmpfs rw\n"
        with self.assertRaisesRegex(ArmError, "external mounts"):
            a.snapshot(0)
        self.assertEqual(w.criu_dumps, 0)
        self.assertFalse(os.path.exists(os.path.join(p["dir"], "upper.snap")))

    def test_provision_fails_closed_when_the_mount_table_needs_external_mounts(self):
        e = CEnv(self)
        a, w, ex = e.arm(CWorld(mountinfo=GOOD_MOUNTINFO + "30 21 8:1 /x /x rw - ext4 /dev/sda1 rw\n"))
        with self.assertRaisesRegex(ArmError, "violates the contract"):
            a.provision(0)
        self.assertEqual(w.trees, {})                                  # the slot was swept

    def test_a_checkpoint_without_network_images_is_refused_before_any_restore(self):
        e, a, w, ex, p = self.provisioned(net_images=("core-2.img",))
        with self.assertRaisesRegex(ArmError, "lacks the network images"):
            a.snapshot(0)
        self.assertEqual(w.restore_argvs, [])

    def test_a_restored_namespace_that_differs_fails_closed_before_the_proxy(self):
        e, a, w, ex, p = self.provisioned(restored_lo_down=True)
        proxies_before = len([x for x in w.events if x[0] == "proxy"])
        with self.assertRaisesRegex(ArmError, "differs from the checkpointed one"):
            a.snapshot(0)
        self.assertEqual(len([x for x in w.events if x[0] == "proxy"]), proxies_before)
        self.assertEqual(w.trees, {})                                  # swept
        self.assertTrue(any(n.startswith("service-logs-restore") or n.startswith("netstate-") for n in os.listdir(p["dir"])))

    def test_a_restored_mount_table_that_differs_fails_closed(self):
        e, a, w, ex, p = self.provisioned(restored_mountinfo=GOOD_MOUNTINFO.replace("/run rw", "/run2 rw"))
        with self.assertRaisesRegex(ArmError, "mount table after"):
            a.snapshot(0)

    def test_host_namespace_means_no_nsenter(self):
        e = CEnv(self)
        a, w, ex = e.arm(CWorld(tree_ns=HOST_NS))
        with self.assertRaises(ArmError):
            a.provision(0)
        self.assertEqual(w.nsenter_calls, [])

    def test_changed_runtime_sources_refuse_snapshot_and_restore(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        mk = os.path.join(p["dir"], cand.SLOT_MARKER)
        m = json.load(open(mk))
        m["source_sha256"] = "0" * 64
        with open(mk, "w") as f:
            json.dump(m, f)
        for verb in ("snapshot", "restore", "reset"):
            with self.assertRaisesRegex(ArmError, "sources changed"):
                getattr(a, verb)(0)
        self.assertEqual(len(w.restore_argvs), 1)

    def test_pid_or_start_time_mismatch_refuses_the_dump(self):
        e, a, w, ex, p = self.provisioned()
        a.start_time = lambda pid: "other"
        with self.assertRaisesRegex(ArmError, "not the process recorded"):
            a.snapshot(0)
        self.assertEqual(w.criu_dumps, 0)

    def test_every_verb_refuses_forged_or_missing_markers(self):
        for how in ("delete_slot_marker", "forge_slot", "delete_root", "symlink_upper"):
            e, a, w, ex, p = self.provisioned()
            base = a.cfg["state_dir"]
            if how == "delete_slot_marker":
                os.remove(os.path.join(p["dir"], cand.SLOT_MARKER))
            elif how == "forge_slot":
                mk = os.path.join(p["dir"], cand.SLOT_MARKER)
                m = json.load(open(mk))
                m["lower_real"] = "/somewhere/else"
                json.dump(m, open(mk, "w"))
            elif how == "delete_root":
                os.remove(os.path.join(base, cand.ROOT_MARKER))
            else:
                shutil.rmtree(p["upper"])
                os.symlink("/", p["upper"])
            calls = len(ex.calls)
            for verb in ("endpoint", "snapshot", "restore", "reset"):
                with self.assertRaises(cand.OwnershipError, msg=(how, verb)):
                    getattr(a, verb)(0)
            self.assertEqual(len(ex.calls), calls, how)                 # nothing was run
            rep = None
            try:
                rep = a.cleanup()
            except cand.OwnershipError:
                pass
            if rep is not None:
                self.assertFalse(rep["ok"])
                self.assertIn("refused", rep["slots"]["slot0"])
            self.assertTrue(os.path.lexists(p["dir"]))                  # nothing removed

    def test_cleanup_sweeps_then_removes_only_owned_heavy_dirs_and_keeps_evidence(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        rep = a.cleanup()
        self.assertTrue(rep["ok"], rep)
        self.assertEqual(w.trees, {})
        self.assertEqual(w.proxies, {})
        for d in ("upper", "upper.snap", "images"):
            self.assertFalse(os.path.exists(os.path.join(p["dir"], d)), d)
        self.assertTrue(os.path.isfile(os.path.join(p["dir"], cand.SLOT_MARKER)))
        self.assertTrue(any(n.startswith("netstate-pre-dump") for n in os.listdir(p["dir"])))

    def test_cleanup_deletes_nothing_when_a_process_cannot_be_stopped(self):
        e, a, w, ex, p = self.provisioned()
        w.f["unkillable"] = set(w.trees)
        rep = a.cleanup()
        self.assertFalse(rep["ok"])
        self.assertTrue(os.path.isdir(p["upper"]))

    def test_cleanup_does_not_touch_foreign_state_dirs(self):
        e = CEnv(self)
        os.makedirs(os.path.join(e.state, "slot0", "upper"))
        a, w, ex = e.arm()
        with self.assertRaises(cand.OwnershipError):
            a.cleanup()
        self.assertTrue(os.path.isdir(os.path.join(e.state, "slot0", "upper")))

    def test_restore_refuses_a_stale_or_mounted_criu_root(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        croot = os.path.join(p["dir"], "criu-root")
        os.makedirs(croot)
        open(os.path.join(croot, "x"), "w").close()
        with self.assertRaisesRegex(ArmError, "empty, unmounted directory"):
            a.restore(0)

    def test_failed_restore_is_swept_and_evidence_kept(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        w.f["restore_fails"] = True
        with self.assertRaises(ArmError):
            a.restore(0)
        self.assertEqual(w.trees, {})

    def test_two_slots_are_independent(self):
        e = CEnv(self)
        a, w, ex = e.arm()
        a.provision(0)
        a.provision(1)
        self.assertNotEqual(w.ns[a._slot(0)["init_pid"]], w.ns[a._slot(1)["init_pid"]])
        a.snapshot(0)
        self.assertEqual(len(w.restore_argvs), 1)
        self.assertEqual(a._slot(1)["state"], "running")

    def test_environment_requires_pinned_criu_and_no_criu_config(self):
        e = CEnv(self)
        open(os.path.join(e.d, "ctl"), "w").write("cpu memory\n")
        a, w, ex = e.arm()
        old = armmod._has_setns
        armmod._has_setns = lambda: True
        self.addCleanup(setattr, armmod, "_has_setns", old)
        self.assertIn("criu: 4.3", a.environment())
        orig = cand.criu_config_files
        cand.criu_config_files = lambda home, env, global_dir="/etc/criu": ["/etc/criu/default.conf"]
        self.addCleanup(setattr, cand, "criu_config_files", orig)
        with self.assertRaisesRegex(ArmError, "configuration files"):
            a.environment()

    def test_describe_labels_the_candidate_and_names_the_contract(self):
        e = CEnv(self)
        a, w, ex = e.arm()
        d = a.describe()
        self.assertEqual(d["arm"], "sharedos-rootfs-candidate")
        self.assertIn("--root", d["criu_restore_argv_template"])
        self.assertIn("NOT a Drums result", d["label"])


class LockTests(unittest.TestCase):
    FLOCK = "1: FLOCK  ADVISORY  WRITE 4242 00:1e:777 0 EOF"          # a lock taken by a task of the tree on a file of its private tmpfs

    def provisioned(self, **flags):
        e = CEnv(self)
        w = CWorld(**flags)
        a, w, ex = e.arm(w)
        a.provision(0)
        return e, a, w, ex, L.slot_paths(a.cfg["state_dir"], 0)

    def set_lock(self, w, pid, fstype_mnt=27, lock=None, path="/tmp/.gnome-lock"):
        w.lock_fds = [(3, lock or self.FLOCK.replace("4242", str(pid)), path, fstype_mnt)]

    def locked(self, **kw):
        e, a, w, ex, p = self.provisioned(**kw)
        pid = a._slot(0)["init_pid"]
        self.set_lock(w, pid)                                              # mnt 27 is the tmpfs /tmp in GOOD_MOUNTINFO
        w.proc_locks = f"1: FLOCK  ADVISORY  WRITE {pid} 00:1e:777 0 EOF\n"
        return e, a, w, ex, p

    def test_a_lock_inside_the_enclosed_tree_adds_file_locks_to_both_dump_and_restore(self):
        e, a, w, ex, p = self.locked()
        a.snapshot(0)
        self.assertIn("--file-locks", w.dump_argvs[0])
        self.assertIn("--file-locks", w.restore_argvs[0])
        self.assertTrue(a._slot(0)["file_locks"])
        census = json.load(open(os.path.join(p["dir"], "locks-census.json")))
        self.assertTrue(census["file_locks_flag"])
        self.assertEqual(census["locks"][0]["path"], "/tmp/.gnome-lock")
        self.assertFalse(census["tree_shape_differs"])
        dumped = json.load(open(os.path.join(p["dir"], "locks-dumped.json")))
        self.assertEqual(len(dumped["dumped"]), 1)
        self.assertEqual(len(dumped["filelocks_img_sha256"]), 64)
        gates = [n for n in os.listdir(p["dir"]) if n.startswith("locks-gates-")]
        self.assertEqual(len(gates), 1)
        a.reset(0)
        self.assertIn("--file-locks", w.restore_argvs[1])
        flat = " ".join(w.dump_argvs[0] + w.restore_argvs[0])
        for bad in ("--skip", "--ignore", "--force", "--enable-fs"):
            self.assertNotIn(bad, flat)

    def test_every_restore_runs_the_read_only_pre_resume_census_hook_from_the_slot_directory(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        argv = w.restore_argvs[0]
        script = argv[argv.index("--action-script") + 1]
        self.assertTrue(script.startswith(os.path.join(p["dir"], "lock-hook-1")))
        body = open(script).read()
        self.assertIn("locks.py", body)
        self.assertIn("--hook", body)
        self.assertEqual(os.stat(script).st_mode & 0o777, 0o700)
        self.assertNotIn("--file-locks", argv)                              # no lock: the census still runs and must be empty

    def test_a_lock_released_after_the_tasks_resume_is_recorded_not_mistaken_for_a_lost_lock(self):
        """The live finding: expected 2 READ locks, found [] AFTER resume. An SQLite reader's shared lock is transient, so the post-resume census is
        evidence only; the gate is applied-vs-dumped and the pre-resume census."""
        e, a, w, ex, p = self.locked()
        w.restored_lock_fds = []                                           # the resumed task already released it
        a.snapshot(0)
        post = [n for n in os.listdir(p["dir"]) if n.startswith("locks-post-resume-")]
        rec = json.load(open(os.path.join(p["dir"], post[0])))
        self.assertEqual((rec["gating"], rec["locks"]), (False, []))
        self.assertEqual(a._slot(0)["state"], "running")

    def test_each_deterministic_lock_fidelity_failure_fails_closed_before_the_proxy(self):
        for mutate, word in ((lambda w: setattr(w, "applied_override", []), "restore applied file locks"),
                             (lambda w: setattr(w, "hook_mode", "missing"), "left no readable census"),
                             (lambda w: setattr(w, "hook_mode", "error"), "pre-resume census failed"),
                             (lambda w: setattr(w, "hook_shape", []), "before the tasks resumed"),
                             (lambda w: setattr(w, "hook_shape", [[1, 0, 5, 5], [1, 0, 1073741826, 510], [1, 0, 128, 1]]), "before the tasks resumed"),
                             (lambda w: setattr(w, "filelocks_changed", True), "filelocks.img changed")):
            e, a, w, ex, p = self.locked()
            w.lock_fds = [(3, "1: POSIX  ADVISORY  READ 4242 00:1e:777 1073741826 1073742335", "/tmp/meta.db", 27),
                          (4, "2: POSIX  ADVISORY  READ 4242 00:1e:778 128 128", "/tmp/meta.db-shm", 27)]
            w.proc_locks = ""
            mutate(w)
            proxies = len([x for x in w.events if x[0] == "proxy"])
            with self.assertRaisesRegex(ArmError, word):
                a.snapshot(0)
            self.assertEqual(len([x for x in w.events if x[0] == "proxy"]), proxies, word)
            self.assertEqual(w.trees, {}, word)                            # swept

    def test_the_live_locks_two_nautilus_read_locks_pass_both_gates(self):
        e, a, w, ex, p = self.provisioned()
        pid = a._slot(0)["init_pid"]
        w.lock_fds = [(3, "1: POSIX  ADVISORY  READ %d 00:1e:777 1073741826 1073742335" % pid, "/home/user/.local/share/nautilus/tags/meta.db", 27),
                      (4, "2: POSIX  ADVISORY  READ %d 00:1e:778 128 128" % pid, "/home/user/.local/share/nautilus/tags/meta.db-shm", 27)]
        a.snapshot(0)
        g = json.load(open([os.path.join(p["dir"], n) for n in os.listdir(p["dir"]) if n.startswith("locks-gates-")][0]))
        self.assertEqual(len(g["dumped"]), 2)
        self.assertEqual(g["applied"], sorted(g["dumped"], key=str))

    def test_the_census_uses_pid_namespace_membership_not_only_the_ppid_graph(self):
        e, a, w, ex, p = self.provisioned()
        pid = a._slot(0)["init_pid"]
        a.ns_members = lambda root: [root, root + 77]                      # a task whose parent is not in the init's descendant graph
        c = a.lock_census(pid, GOOD_MOUNTINFO)
        self.assertTrue(c["tree_shape_differs"])
        self.assertIn(pid + 77, c["tree_pids"])

    def test_a_lock_on_a_non_private_filesystem_refuses_the_dump_before_criu_runs(self):
        e, a, w, ex, p = self.provisioned()
        pid = a._slot(0)["init_pid"]
        self.set_lock(w, pid, fstype_mnt=22)                               # mnt 22 is proc
        with self.assertRaisesRegex(ArmError, "enclosure condition"):
            a.snapshot(0)
        self.assertEqual(w.criu_dumps, 0)
        self.assertEqual(w.dump_argvs, [])

    def test_a_lock_also_held_or_awaited_by_a_process_outside_the_tree_refuses_the_dump(self):
        for line in ("1: FLOCK  ADVISORY  WRITE 999 00:1e:777 0 EOF", "2: -> FLOCK  ADVISORY  WRITE 999 00:1e:777 0 EOF"):
            e, a, w, ex, p = self.provisioned()
            pid = a._slot(0)["init_pid"]
            self.set_lock(w, pid)
            w.proc_locks = f"1: FLOCK  ADVISORY  WRITE {pid} 00:1e:777 0 EOF\n{line}\n"
            with self.assertRaisesRegex(ArmError, "outside the dumped tree"):
                a.snapshot(0)
            self.assertEqual(w.dump_argvs, [])

    def test_leases_and_mandatory_locks_are_not_covered(self):
        for lock in ("1: LEASE  ADVISORY  READ 4242 00:1e:777 0 EOF", "1: FLOCK  MSNFS  READ 4242 00:1e:777 0 EOF"):
            e, a, w, ex, p = self.provisioned()
            pid = a._slot(0)["init_pid"]
            self.set_lock(w, pid, lock=lock.replace("4242", str(pid)))
            with self.assertRaisesRegex(ArmError, "enclosure condition"):
                a.snapshot(0)

    def test_the_lock_files_are_part_of_the_source_hash(self):
        self.assertIn("locks.py", cand.SOURCE_FILES)


class LockModuleTests(unittest.TestCase):
    LOG = """(00.100) Dumping task (pid: 3001 comm: gnome-shell)
(00.101) Collecting fds (pid: 3001)
(00.102) lockinfo: 5:2 1 3001 00:2e:777 0 EOF
(00.200) Dumping task (pid: 3050 comm: dconf-service)
(00.201) Collecting fds (pid: 3050)
(00.202) lockinfo: 9:1 1 3050 08:01:4242 0 100
(00.900) Error (criu/file-lock.c:112): Some file locks are hold by dumping tasks! You can try --file-locks to dump them.
"""

    def test_scan_log_names_the_task_comm_inode_and_kind(self):
        from sharedos_rootfs import locks
        found = locks.scan_log(self.LOG)
        self.assertEqual([(f["collecting_pid"], f["comm"], f["kind"], f["dev"], f["ino"]) for f in found],
                         [(3001, "gnome-shell", "flock", "0:46", 777), (3050, "dconf-service", "posix", "8:1", 4242)])

    def test_find_paths_is_offline_best_effort(self):
        from sharedos_rootfs import locks
        d = tmp(self)
        p = os.path.join(d, "a", "lockfile")
        os.makedirs(os.path.dirname(p))
        open(p, "w").close()
        ino = os.stat(p).st_ino
        hits = locks.find_paths("0:0", ino, [d])
        self.assertEqual(hits[0]["path"], p)

    def test_cli_prints_json_and_exit_code(self):
        import contextlib
        from sharedos_rootfs import locks
        d = tmp(self)
        log = os.path.join(d, "dump.log")
        open(log, "w").write(self.LOG)
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            self.assertEqual(locks.main([log]), 0)
        self.assertEqual(len(json.loads(out.getvalue())), 2)
        open(log, "w").write("(00.1) nothing\n")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(locks.main([log]), 1)

    def test_the_real_hook_script_sees_a_real_posix_read_lock_through_proc(self):
        """No fakes: a child holds a POSIX read lock on a real file; the generated hook script, run as CRIU would (action + init pid in the
        environment), writes a census that finds it in the same shape CRIU's own record uses."""
        import subprocess
        d = tmp(self)
        target = os.path.join(d, "meta.db")
        open(target, "w").write("x" * 4096)
        child = subprocess.Popen([sys.executable, "-c", "import fcntl,os,sys,time\nf=open(sys.argv[1],'r+b')\nfcntl.lockf(f, fcntl.LOCK_SH, 510, 1073741826, 0)\n"
                                  "print('locked', flush=True); time.sleep(60)", target], stdout=subprocess.PIPE, text=True)
        self.addCleanup(child.kill)
        self.assertEqual(child.stdout.readline().strip(), "locked")
        e = CEnv(self)
        a, w, ex = e.arm()
        os.makedirs(os.path.join(e.state), exist_ok=True)
        script, hook_dir = a._write_hook(0, {"dir": d}, 1)
        for action in ("pre-resume", "post-resume"):
            r = subprocess.run([script], env={"PATH": os.environ["PATH"], "CRTOOLS_SCRIPT_ACTION": action, "CRTOOLS_INIT_PID": str(child.pid)}, capture_output=True, text=True)
            self.assertEqual(r.returncode, 0, r.stderr)
        rec = json.load(open(os.path.join(hook_dir, "locks-pre-resume.json")))
        mine = [l for l in rec["locks"] if l["path"] == target]
        self.assertEqual(len(mine), 1, rec.get("error"))
        self.assertEqual((mine[0]["kind"], mine[0]["type"], mine[0]["start"], mine[0]["end"]), ("posix", "READ", 1073741826, "1073742335"))
        self.assertIn([1, 0, 1073741826, 510], rec["shape"])
        self.assertIn(child.pid, rec["by_pid_ns"])
        self.assertTrue(os.path.exists(os.path.join(hook_dir, "locks-post-resume.json")))
        r = subprocess.run([script], env={"PATH": os.environ["PATH"], "CRTOOLS_SCRIPT_ACTION": "post-dump"}, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0)                                      # any other action: does nothing

    def test_a_hook_with_no_init_pid_records_an_error_for_the_gate(self):
        from sharedos_rootfs import locks
        d = tmp(self)
        self.assertEqual(locks.hook_main(d, env={"CRTOOLS_SCRIPT_ACTION": "pre-resume"}), 0)
        self.assertIn("error", json.load(open(os.path.join(d, "locks-pre-resume.json"))))

    def test_parse_and_contract_edge_cases(self):
        from sharedos_rootfs import locks
        with self.assertRaises(locks.LockError):
            locks.parse_lock("garbage")
        w = locks.parse_lock("2: -> POSIX  ADVISORY  READ 77 00:2d:99 0 4")
        self.assertTrue(w["blocked"])
        ok = {"id": 1, "kind": "ofd", "advisory": "ADVISORY", "type": "WRITE", "pid": 1, "fd": 3, "path": "/x", "mnt_id": 21}
        self.assertEqual(locks.contract([ok], {21: "overlay"}, []), [])
        self.assertTrue(locks.contract([ok], {21: "ext4"}, []))
        self.assertTrue(locks.contract([{**ok, "kind": "lease"}], {21: "overlay"}, []))


class CliTests(unittest.TestCase):
    def test_describe_and_unprovisioned_verbs_through_the_real_cli(self):
        import subprocess
        e = CEnv(self)
        e.arm()                                                       # writes the config
        env = dict(os.environ, SHAREDOS_ROOTFS_CONFIG=e.cfg_path, PYTHONPATH=os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
        r = subprocess.run([sys.executable, "-m", "sharedos_rootfs.candidate", "describe"], env=env, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        d = json.loads(r.stdout)
        self.assertEqual(d["arm"], "sharedos-rootfs-candidate")
        self.assertEqual(d["source_sha256"], cand.source_sha256())
        for verb in ("endpoint", "snapshot", "restore"):
            r = subprocess.run([sys.executable, "-m", "sharedos_rootfs.candidate", verb, "--slot", "0"], env=env, capture_output=True, text=True)
            self.assertEqual(r.returncode, 1, verb)
            self.assertIn("arm error", r.stderr)


if __name__ == "__main__":
    unittest.main()
