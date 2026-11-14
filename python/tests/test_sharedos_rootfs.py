"""Tests for the rootfs slot runtime. FAKE root filesystems, process worlds and executors: they check control
flow, fail-closed behaviour and cleanup, NOT that the real OSWorld image runs or that CRIU can checkpoint it."""

import contextlib
import hashlib
import io
import json
import os
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))
from rootfs_helpers import SERVER_SRC, FakeHost, FakeUserns, Res, ScriptExec, World, arm_exec, fake_cgroup_reader, make_rootfs  # noqa: E402

from osworld_smoke import pins  # noqa: E402
from sharedos_rootfs import FIDELITY_DIFFERENCES, PINNED_SERVER_MAIN_SHA256  # noqa: E402
from sharedos_rootfs import arm as armmod  # noqa: E402
from sharedos_rootfs import display_stage as ds  # noqa: E402
from sharedos_rootfs import image as img  # noqa: E402
from sharedos_rootfs import launch as L  # noqa: E402
from sharedos_rootfs import proxy as px  # noqa: E402
from sharedos_rootfs import rootfs_inspect as ri  # noqa: E402
from sharedos_rootfs import sameness as sm  # noqa: E402
from sharedos_rootfs import supervisor as sv  # noqa: E402
from sharedos_rootfs import tree as tr  # noqa: E402
from sharedos_rootfs import harden as H  # noqa: E402
from sharedos_rootfs import criu_root as cr  # noqa: E402

PKG_ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
NO_SLEEP = lambda s: None  # noqa: E731


def tmp(tc):
    d = tempfile.mkdtemp()
    tc.addCleanup(shutil.rmtree, d, ignore_errors=True)
    return d


# ------------------------------------------------------------------------------------------- image
class ImageSha(unittest.TestCase):
    def test_verify_matches_mismatches_and_caches(self):
        d = tmp(self)
        p = os.path.join(d, "i.qcow2")
        open(p, "wb").write(b"abc")
        good = hashlib.sha256(b"abc").hexdigest()
        calls = []

        def hasher(path):
            calls.append(path)
            return hashlib.sha256(open(path, "rb").read()).hexdigest()
        c = os.path.join(d, "cache")
        self.assertFalse(img.verify_image(p, good, c, hasher)["from_cache"])
        self.assertTrue(img.verify_image(p, good, c, hasher)["from_cache"])
        self.assertEqual(len(calls), 1)
        os.utime(p, (1, 1))                       # same bytes, different mtime: rehash, never trust a stale cache
        self.assertFalse(img.verify_image(p, good, c, hasher)["from_cache"])
        with self.assertRaises(img.ImageError):
            img.verify_image(p, "0" * 64, os.path.join(d, "c2"), hasher)
        self.assertFalse(os.path.exists(os.path.join(d, "c2")))     # a mismatch is never cached
        for bad in ("", "ABC", "x" * 64, None):
            with self.assertRaises(img.ImageError):
                img.verify_image(p, bad, c, hasher)


class RootPartition(unittest.TestCase):
    def ls(self, *parts):
        return {"blockdevices": [{"name": "nbd0", "type": "disk", "children": [
            {"name": n, "size": s, "fstype": f, "type": "part"} for n, s, f in parts]}]}

    def test_picks_the_largest_root_filesystem(self):
        self.assertEqual(img.pick_root_partition(self.ls(("nbd0p1", 100, "vfat"), ("nbd0p2", 5000, "ext4"), ("nbd0p3", 800, "swap"))), "nbd0p2")

    def test_refuses_none_or_ambiguous(self):
        with self.assertRaises(img.ImageError):
            img.pick_root_partition(self.ls(("nbd0p1", 100, "vfat")))
        with self.assertRaises(img.ImageError):
            img.pick_root_partition(self.ls(("nbd0p1", 1000, "ext4"), ("nbd0p2", 950, "ext4")))
        with self.assertRaises(img.ImageError):
            img.pick_root_partition({"blockdevices": []})


def image_exec(fail=None, busy=False, rw=False, still_mounted=False, disconnect_busy=False, partitions=True):
    st = {"connected": False, "mounted": False}
    lsblk = json.dumps({"blockdevices": [{"name": "nbd0", "type": "disk", "children": [
        {"name": "nbd0p1", "size": 24 * 10**9, "fstype": "ext4", "type": "part"}]}]}) if partitions else json.dumps({"blockdevices": []})

    def fn(argv):
        n = argv[0]
        if fail and fail == n:
            return Res(1, "", f"{n} exploded")
        if n == "test":
            return Res(0 if (busy or st["connected"]) else 1) if not disconnect_busy or st["connected"] or True else Res(1)
        if n == "qemu-nbd" and argv[1] == "--disconnect":
            st["connected"] = disconnect_busy
            return Res(0)
        if n == "qemu-nbd":
            st["connected"] = True
            return Res(0)
        if n == "lsblk":
            return Res(0, lsblk)
        if n == "mount":
            st["mounted"] = True
            return Res(0)
        if n == "umount":
            st["mounted"] = still_mounted
            return Res(0)
        if n == "findmnt" and "-o" in argv:
            return Res(0, "rw,relatime\n" if rw else "ro,nodev,nosuid\n")
        if n == "findmnt":
            return Res(0 if st["mounted"] else 1)
        return None
    ex = ScriptExec(fn)
    ex.state = st
    return ex


class ImageMountTests(unittest.TestCase):
    def mount(self, **kw):
        ex = image_exec(**kw)
        return img.ImageMount("/data/Ubuntu.qcow2", "/mnt/os", ex, sleep=NO_SLEEP, settle_attempts=2), ex

    def test_attach_is_read_only_in_every_command_and_reads_the_option_back(self):
        m, ex = self.mount()
        info = m.attach()
        nbd = next(c for c in ex.calls if c[0] == "qemu-nbd")
        self.assertIn("--read-only", nbd)
        self.assertIn("--connect=/dev/nbd0", nbd)
        self.assertEqual(nbd[-1], "/data/Ubuntu.qcow2")
        mnt = next(c for c in ex.calls if c[0] == "mount")
        self.assertEqual(mnt[:3], ["mount", "-o", "ro,noload,nodev,nosuid"])
        self.assertEqual(info["partition"], "/dev/nbd0p1")
        self.assertEqual(ex.names()[:2], ["modprobe", "test"])           # busy check before attaching
        self.assertLess(ex.names().index("qemu-nbd"), ex.names().index("mount"))

    def test_a_busy_nbd_device_is_never_touched(self):
        m, ex = self.mount(busy=True)
        with self.assertRaises(img.ImageError):
            m.attach()
        self.assertNotIn("qemu-nbd", ex.names())

    def test_a_read_write_mount_is_refused(self):
        m, ex = self.mount(rw=True)
        with self.assertRaises(img.ImageError) as cm:
            m.attach()
        self.assertIn("not read-only", str(cm.exception))

    def test_failed_attach_steps_clean_up_through_the_context_manager(self):
        for fail in ("mount", "lsblk", "mkdir"):
            m, ex = self.mount(fail=fail)
            with self.assertRaises(img.ImageError):
                with m:
                    pass
            self.assertFalse(ex.state["connected"], fail)   # no connected device left behind
            self.assertFalse(ex.state["mounted"], fail)
            if fail != "lsblk" or True:
                self.assertIn("qemu-nbd", ex.names())

    def test_no_partition_ever_appearing_is_an_error_and_disconnects(self):
        m, ex = self.mount(partitions=False)
        with self.assertRaises(img.ImageError):
            with m:
                pass
        self.assertFalse(ex.state["connected"])

    def test_clean_exit_unmounts_then_disconnects_and_verifies(self):
        m, ex = self.mount()
        with m:
            pass
        n = ex.names()
        self.assertLess(n.index("umount"), n.index("qemu-nbd", n.index("umount")))
        self.assertEqual((ex.state["mounted"], ex.state["connected"]), (False, False))

    def test_unmount_failure_leaves_the_device_connected_and_reports_evidence(self):
        m, ex = self.mount(still_mounted=True)
        m.attach()
        with self.assertRaises(img.CleanupError) as cm:
            m.detach()
        self.assertTrue(cm.exception.evidence)
        self.assertEqual([c for c in ex.calls if c[:2] == ["qemu-nbd", "--disconnect"]], [])   # unsafe to disconnect

    def test_disconnect_failure_is_reported(self):
        m, ex = self.mount(disconnect_busy=True)
        m.attach()
        with self.assertRaises(img.CleanupError):
            m.detach()


# ---------------------------------------------------------------------------------------- inspect
class InspectTests(unittest.TestCase):
    def root(self, **kw):
        return make_rootfs(tmp(self), **kw)

    def test_a_complete_image_is_ok_and_describes_how_it_starts_the_server(self):
        r = ri.inspect_rootfs(self.root())
        self.assertTrue(r["ok_for_server"], r["blockers"])
        self.assertEqual(r["os_release"]["VERSION_ID"], "22.04")
        self.assertEqual(r["user"]["name"], "user")
        self.assertEqual(r["server"]["path"], "/home/user/server/main.py")
        self.assertIn("screen_recording.py", r["server"]["siblings"])
        self.assertEqual(r["launchers"][0]["exec"], ["/usr/bin/python3 /home/user/server/main.py"])
        self.assertEqual(r["launchers"][0]["working_directory"], ["/home/user/server"])
        self.assertTrue(r["autologin"]["enabled"])
        self.assertTrue(r["desktop_dir"])
        self.assertTrue(r["gnome_shell_present"])
        self.assertEqual(r["display"]["decision"], "xvfb_in_image")

    def test_server_hash_is_compared_with_the_pinned_public_source(self):
        root = self.root()
        r = ri.inspect_rootfs(root)
        self.assertFalse(r["server_matches_pinned_public_source"])      # fake content != real source
        want = hashlib.sha256(open(os.path.join(root, "home/user/server/main.py"), "rb").read()).hexdigest()
        with mock.patch.object(ri, "PINNED_SERVER_MAIN_SHA256", want):
            self.assertTrue(ri.inspect_rootfs(root)["server_matches_pinned_public_source"])
        self.assertEqual(len(PINNED_SERVER_MAIN_SHA256), 64)

    def test_each_missing_prerequisite_blocks(self):
        for kw, text in (({"user": False}, "no 'user' account"), ({"server": False, "unit": False}, "no OSWorld guest server"),
                         ({"second_server": True}, "ambiguous"), ({"pyautogui": False}, "pyautogui"),
                         ({"flask": False}, "flask"), ({"xlib": False}, "Xlib"), ({"dbus": False}, "dbus-daemon"),
                         ({"python3": False}, "python3"), ({"os_release": False}, "os-release")):
            r = ri.inspect_rootfs(self.root(**kw))
            self.assertFalse(r["ok_for_server"], kw)
            self.assertTrue(any(text in b for b in r["blockers"]), (kw, r["blockers"]))

    def test_display_decision_follows_what_is_really_in_the_image(self):
        self.assertEqual(ri.inspect_rootfs(self.root(xvfb=False, xorg=True, dummy=True))["display"]["decision"], "xorg_dummy_in_image")
        r = ri.inspect_rootfs(self.root(xvfb=False, xorg=True, dummy=False))
        self.assertEqual(r["display"]["decision"], "needs_added_display_package")
        self.assertEqual(r["display"]["added_packages_needed"], ["xvfb"])
        self.assertEqual(ri.inspect_rootfs(self.root(xvfb=False))["display"]["decision"], "needs_added_display_package")

    def test_exec_line_parsing(self):
        self.assertEqual(ri.exec_argv("-/usr/bin/python3 /home/user/server/main.py"), ["/usr/bin/python3", "/home/user/server/main.py"])
        self.assertEqual(ri.exec_argv("/bin/sh -c 'echo hi %f'"), ["/bin/sh", "-c", "echo hi "])
        self.assertEqual(ri.exec_argv("@/usr/bin/x y"), ["/usr/bin/x", "y"])

    def test_autostart_desktop_entries_count_as_launchers(self):
        root = self.root(unit=False)
        p = os.path.join(root, "home/user/.config/autostart/osworld.desktop")
        os.makedirs(os.path.dirname(p))
        open(p, "w").write("[Desktop Entry]\nExec=python3 /home/user/server/main.py\n")
        r = ri.inspect_rootfs(root)
        self.assertEqual(r["launchers"][0]["exec"], ["python3 /home/user/server/main.py"])

    def test_cli_exit_codes_and_no_overwrite(self):
        good, bad = self.root(), self.root(user=False)
        out = os.path.join(tmp(self), "i.json")
        run = lambda root, o: subprocess.run([sys.executable, "-m", "sharedos_rootfs.rootfs_inspect", root, "--out", o],  # noqa: E731
                                             cwd=PKG_ROOT, capture_output=True, text=True)
        r = run(good, out)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(json.load(open(out))["ok_for_server"])
        self.assertNotEqual(run(good, out).returncode, 0)            # refuses to overwrite
        r2 = run(bad, os.path.join(tmp(self), "j.json"))
        self.assertEqual(r2.returncode, 1)
        self.assertIn("no 'user' account", r2.stdout)


# ---------------------------------------------------------------------------------------- launch
class LaunchTests(unittest.TestCase):
    def test_marker_is_stable_and_distinct_per_state_dir_and_slot(self):
        a = L.marker_for("/s/a", 0)
        self.assertEqual(a, L.marker_for("/s/a", 0))
        self.assertNotEqual(a, L.marker_for("/s/a", 1))
        self.assertNotEqual(a, L.marker_for("/s/b", 0))

    def test_cgroup_properties_equal_the_baseline_guest_resources(self):
        self.assertEqual(L.cgroup_properties(4, 4096), ["CPUQuota=400%", "MemoryMax=4096M", "MemorySwapMax=0", "TasksMax=8192"])

    def test_launch_argv_scopes_detaches_and_namespaces(self):
        a = L.launch_argv("m1", "/p/plan.json", "/p/log", 4, 4096, slot=2)
        self.assertEqual(a[:5], ("systemd-run", "--scope", "--collect", "--quiet", "--unit"))
        self.assertIn("CPUQuota=400%", a)
        self.assertIn("MemoryMax=4096M", a)
        self.assertIn("</dev/null", a[a.index("-c") + 1])
        for flag in ("--pid", "--fork", "--mount", "--net", "--ipc", "--uts"):
            self.assertIn(flag, a)
        self.assertEqual(a[a.index("--propagation") + 1], "private")
        self.assertEqual(a[-4:], ("--marker", "m1", "--plan", "/p/plan.json"))

    def test_launch_argv_puts_the_slot_in_a_user_namespace_with_an_unprivileged_map(self):
        # the fix: root-in-slot must not be root-on-host. The user namespace is named before the other namespaces
        # (which it must own), slot root (inner 0) maps to an unprivileged host uid, and the init runs as root inside.
        import sharedos_rootfs.harden as H
        a = L.launch_argv("m1", "/p/plan.json", "/p/log", 4, 4096, slot=2)
        self.assertIn("--user", a)
        self.assertLess(a.index("--user"), a.index("--pid"))
        self.assertEqual(a[a.index("--map-users") + 1], f"{H.SUBID_BASE + 2 * H.SUBID_COUNT},0,{H.SUBID_COUNT}")
        self.assertEqual(a[a.index("--map-groups") + 1], f"{H.SUBID_BASE + 2 * H.SUBID_COUNT},0,{H.SUBID_COUNT}")
        self.assertEqual(a[a.index("--setuid") + 1], "0")
        self.assertEqual(a[a.index("--setgid") + 1], "0")
        # disjoint slots never share a host uid range
        b = L.launch_argv("m2", "/p/plan.json", "/p/log", 4, 4096, slot=3)
        self.assertNotEqual(a[a.index("--map-users") + 1], b[b.index("--map-users") + 1])

    def test_criu_defaults_stop_the_tree_so_the_disk_copy_is_consistent(self):
        self.assertNotIn("--leave-running", L.dump_argv(7, "/i", "/l"))
        self.assertIn("--leave-running", L.dump_argv(7, "/i", "/l", leave_running=True))
        # dconf replaces its database by rename while clients map the old inode: the dump must be allowed to link-remap it
        self.assertIn("--link-remap", L.dump_argv(7, "/i", "/l"))
        self.assertIn("--link-remap", L.dump_argv(7, "/i", "/l", leave_running=True, file_locks=True))
        self.assertNotIn("--link-remap", L.restore_argv("/i", "/p", "/l"))

    def test_nested_pidns_on_dump_and_restore(self):
        # sandboxed apps (Chrome, Electron) nest pid/user/net namespaces below the slot's: both sides name --nested-pidns
        self.assertIn("--nested-pidns", L.dump_argv(7, "/i", "/l"))
        self.assertIn("--nested-pidns", L.dump_argv(7, "/i", "/l", leave_running=True, file_locks=True))
        self.assertIn("--nested-pidns", L.restore_argv("/i", "/p", "/l"))

    def test_restart_interrupted_syscalls_on_dump_only(self):
        # VS Code's file watcher fails on EINTR from poll(); the option belongs to dump
        self.assertIn("--restart-interrupted-syscalls", L.dump_argv(7, "/i", "/l", leave_running=True))
        self.assertNotIn("--restart-interrupted-syscalls", L.restore_argv("/i", "/p", "/l"))

    def test_ghost_limit_on_dump_only(self):
        # deleted /dev/shm files of Chrome are larger than criu's 1 MiB default
        self.assertIn("--ghost-limit=512M", L.dump_argv(7, "/i", "/l", leave_running=True))
        self.assertFalse(any(a.startswith("--ghost-limit") for a in L.restore_argv("/i", "/p", "/l")))
        r = L.restore_argv("/i", "/p", "/l")
        self.assertIn("--restore-detached", r)
        self.assertEqual(r[r.index("--pidfile") + 1], "/p")

    def test_slot_paths_stay_under_the_state_dir(self):
        p = L.slot_paths("/state", 3)
        self.assertTrue(all(v.startswith("/state/slot3") for v in p.values()))


# ------------------------------------------------------------------------------------------ tree
def plan_for(root=None, **over):
    insp = ri.inspect_rootfs(root or make_rootfs(tempfile.mkdtemp()), pins.IMAGE_QCOW_SHA256)
    cfg = {"state_dir": "/s", "lower": "/mnt/os", "inspection": "", "image_sha256": pins.IMAGE_QCOW_SHA256, "vcpus": 4,
           "memory_mib": 4096, "screen": "1920x1080x24", "profile": "gnome_x11", "service_base_port": 26000,
           "guest_service_port": 5000, "_inspection": insp}
    cfg.update(over)
    return armmod.build_plan(cfg, 0), insp


class TreeTests(unittest.TestCase):
    def test_mount_steps_are_ordered_private_and_never_expose_host_devices_wholesale(self):
        plan, _ = plan_for()
        steps = tr.mount_steps(plan)
        runs = [s[1] for s in steps if s[0] == "run"]
        self.assertEqual(runs[0], ["mount", "--make-rprivate", "/"])
        overlay = next(r for r in runs if "overlay" in r)
        self.assertIn(f"lowerdir=/mnt/os,upperdir={plan['upper']},workdir={plan['work']}", overlay)
        flat = [" ".join(r) for r in runs]
        self.assertTrue(any("devpts" in f and "newinstance" in f for f in flat))          # private pty instance
        self.assertTrue(any(f.startswith("mount -t sysfs -o ro") for f in flat))
        self.assertFalse(any("--rbind" in f or "--rbind" in f.split() for f in flat))
        self.assertFalse(any("--bind" in f.split() for f in flat))                       # no host device file is bind-mounted at all
        self.assertTrue(any(f.endswith(plan["merged"] + "/tmp") and "tmpfs" in f for f in flat))
        self.assertLess(flat.index(next(f for f in flat if "overlay" in f)), flat.index(next(f for f in flat if "devpts" in f)))
        self.assertIn(("symlink", "pts/ptmx", plan["merged"] + "/dev/ptmx"), steps)

    # ---- CRIU-safe /dev: native nodes on the private tmpfs -------------------------------------------
    def test_dev_has_native_nodes_with_exact_modes_and_owners_and_no_host_binds(self):
        # real CRIU 4.3 refused the bind-mounted host device files: "mnt:403:./dev/tty doesn't have a proper root mount"
        # (criu/mount.c validate_mounts: a bind of ONE file of the host's devtmpfs has a non-root fs root and no root mount in
        # the slot). A plain tmpfs /dev with real nodes is dumped/restored by CRIU's tmpfs path (tar keeps device nodes).
        plan, _ = plan_for()
        steps = tr.mount_steps(plan)
        m = plan["merged"]
        nodes = {s[1]: s[2:] for s in steps if s[0] == "mknod"}
        self.assertEqual(nodes, {f"{m}/dev/null": (0o666, 1, 3, 0, 0), f"{m}/dev/zero": (0o666, 1, 5, 0, 0),
                                 f"{m}/dev/full": (0o666, 1, 7, 0, 0), f"{m}/dev/random": (0o666, 1, 8, 0, 0),
                                 f"{m}/dev/urandom": (0o666, 1, 9, 0, 0), f"{m}/dev/tty": (0o666, 5, 0, 0, 5)})   # tty: root:tty
        self.assertFalse([s for s in steps if s[0] == "touch"])
        for link, target in (("fd", "/proc/self/fd"), ("stdin", "/proc/self/fd/0"), ("stdout", "/proc/self/fd/1"),
                             ("stderr", "/proc/self/fd/2")):
            self.assertIn(("symlink", target, f"{m}/dev/{link}"), steps)
        kinds = ["run:" + s[1][-1] if s[0] == "run" else s[0] for s in steps]
        self.assertLess(kinds.index(f"run:{m}/dev"), kinds.index("mknod"))        # nodes are created AFTER the private tmpfs /dev
        devpts = next(s[1] for s in steps if s[0] == "run" and "devpts" in s[1])
        self.assertIn("gid=5", devpts[4])
        plan["tty_gid"] = 7                                                      # the gid comes from the image's /etc/group
        steps7 = tr.mount_steps(plan)
        self.assertEqual({s[1]: s[-1] for s in steps7 if s[0] == "mknod"}[f"{m}/dev/tty"], 7)
        self.assertIn("gid=7", next(s[1] for s in steps7 if s[0] == "run" and "devpts" in s[1])[4])

    @unittest.skipUnless(os.geteuid() == 0, "needs root for mknod")
    def test_ops_mknod_makes_a_real_char_device_with_exact_mode_and_owner_despite_umask(self):
        d = tmp(self)
        old = os.umask(0o077)
        try:
            try:
                tr.Ops().mknod(os.path.join(d, "null"), 0o666, 1, 3, 0, 0)
                tr.Ops().mknod(os.path.join(d, "tty"), 0o666, 5, 0, 0, 5)
            except PermissionError:
                self.skipTest("this sandbox cannot mknod")
        finally:
            os.umask(old)
        for name, (maj, mn, gid) in (("null", (1, 3, 0)), ("tty", (5, 0, 5))):
            st = os.stat(os.path.join(d, name))
            self.assertTrue(stat.S_ISCHR(st.st_mode))
            self.assertEqual((os.major(st.st_rdev), os.minor(st.st_rdev)), (maj, mn))
            self.assertEqual(oct(st.st_mode & 0o7777), "0o666")
            self.assertEqual((st.st_uid, st.st_gid), (0, gid))
        tr.Ops().mknod(os.path.join(d, "null"), 0o666, 1, 3, 0, 0)         # idempotent

    # ---- image-native supplementary groups and hostname -------------------------------------------------
    def test_the_inspection_reads_groups_hostname_and_tty_from_the_images_own_files(self):
        insp = ri.inspect_rootfs(make_rootfs(tmp(self)))
        self.assertEqual(insp["groups"], {"user_supplementary": [{"name": "sudo", "gid": 27}, {"name": "cdrom", "gid": 24}],
                                          "tty_gid": 5})
        self.assertEqual(insp["hostname"]["etc_hostname"], "osworld-vm")
        self.assertTrue(insp["hostname"]["etc_hosts_maps_it"])
        r = make_rootfs(tmp(self))
        open(os.path.join(r, "etc/hosts"), "w").write("127.0.0.1 localhost\n")
        self.assertFalse(ri.inspect_rootfs(r)["hostname"]["etc_hosts_maps_it"])
        self.assertEqual(len(ri.inspect_rootfs(r)["hostname"]["etc_hosts_sha256"]), 64)

    def test_the_plan_carries_the_image_groups_hostname_and_tty_gid(self):
        plan, _ = plan_for()
        # the image's sudo membership is not applied: the agent acts as this user and gets no privilege escalation
        self.assertEqual(plan["user"]["groups"], [24])
        self.assertEqual(plan["user"]["group_names"], ["cdrom"])
        self.assertEqual(plan["user"]["dropped_admin_groups"], ["sudo"])
        self.assertEqual((plan["tty_gid"], plan["hostname"], plan["hostname_mapped_in_hosts"]), (5, "osworld-vm", True))




    # ---- no privilege escalation for the agent ------------------------------------------------------------
    def test_the_plan_and_the_slot_use_the_same_admin_groups(self):
        self.assertEqual(armmod.ADMIN_GROUPS, tr.ADMIN_GROUPS)
        self.assertTrue({"sudo", "admin", "wheel", "lxd"} <= set(tr.ADMIN_GROUPS))

    def test_strip_member_takes_the_user_out_of_admin_groups_only(self):
        group = "root:x:0:\nadm:x:4:syslog,user\nsudo:x:27:user\nadmin:x:118:alice,user,bob\ncdrom:x:24:user\nwheel:x:10:\n"
        out = tr.strip_member(group, tr.ADMIN_GROUPS, "user", (3,))
        self.assertEqual(out, "root:x:0:\nadm:x:4:syslog,user\nsudo:x:27:\nadmin:x:118:alice,bob\ncdrom:x:24:user\nwheel:x:10:\n")
        gshadow = "sudo:*:user:user\ncdrom:*::user\nadmin:!:alice:user\n"
        self.assertEqual(tr.strip_member(gshadow, tr.ADMIN_GROUPS, "user", (2, 3)), "sudo:*::\ncdrom:*::user\nadmin:!:alice:\n")
        self.assertEqual(tr.strip_member(group, tr.ADMIN_GROUPS, "nobody", (3,)), group)      # nothing else changes

    def test_strip_admin_groups_rewrites_the_slots_files_in_place_and_fails_closed(self):
        m = tmp(self)
        os.makedirs(os.path.join(m, "etc"))
        g, gs = os.path.join(m, "etc/group"), os.path.join(m, "etc/gshadow")
        open(g, "w").write("sudo:x:27:user\ncdrom:x:24:user\n")
        open(gs, "w").write("sudo:*::user\n")
        os.chmod(g, 0o644)
        os.chmod(gs, 0o640)
        ino = os.stat(g).st_ino
        tr.Ops().strip_admin_groups(m, "user", tr.ADMIN_GROUPS)
        self.assertEqual(open(g).read(), "sudo:x:27:\ncdrom:x:24:user\n")
        self.assertEqual(open(gs).read(), "sudo:*::\n")
        self.assertEqual((oct(os.stat(g).st_mode & 0o777), oct(os.stat(gs).st_mode & 0o777), os.stat(g).st_ino), ("0o644", "0o640", ino))
        # an image without a gshadow is fine; one where the user cannot be removed stops the slot
        os.unlink(gs)
        tr.Ops().strip_admin_groups(m, "user", tr.ADMIN_GROUPS)
        with mock.patch.object(tr, "strip_member", lambda text, *a: text):
            open(g, "w").write("sudo:x:27:user\n")
            with self.assertRaises(tr.PlanError):
                tr.Ops().strip_admin_groups(m, "user", tr.ADMIN_GROUPS)

    def test_a_plan_that_gives_the_user_an_admin_group_is_refused(self):
        plan, _ = plan_for()
        plan["user"]["groups"], plan["user"]["group_names"] = [27, 24], ["sudo", "cdrom"]
        with self.assertRaises(tr.PlanError) as cm:
            tr.services_plan(plan)
        self.assertIn("privilege escalation", str(cm.exception))

    def test_bring_up_strips_the_admin_groups_before_the_pivot(self):
        plan, _ = plan_for()
        ops = StageOps()
        tr.bring_up(plan, ops, "m", getpid=lambda: 1)
        names = [e[0] for e in ops.events]
        self.assertIn(("strip_admin_groups", plan["merged"], "user", tr.ADMIN_GROUPS), ops.events)
        self.assertLess(names.index("strip_admin_groups"), names.index("pivot"))

    @unittest.skipUnless(os.path.exists("/proc/self/status"), "reads NoNewPrivs from /proc")
    def test_no_new_privs_is_set_for_the_process_and_its_children(self):
        code = ("import sys; sys.path.insert(0, %r); import supervisor as sv; sv.no_new_privs(); import subprocess;"
                "print(open('/proc/self/status').read().split('NoNewPrivs:')[1].split()[0],"
                "subprocess.run(['grep', 'NoNewPrivs', '/proc/self/status'], capture_output=True, text=True).stdout.split()[1])"
                % os.path.join(PKG_ROOT, "sharedos_rootfs"))
        out = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=30)
        self.assertEqual(out.stdout.split(), ["1", "1"], out.stderr)

    def test_every_user_process_the_supervisor_starts_drops_to_the_user_then_sets_no_new_privs(self):
        with open(os.path.join(PKG_ROOT, "sharedos_rootfs", "supervisor.py")) as f:
            src = f.read()
        drops = src.split("def drop():")[1:]
        self.assertEqual(len(drops), 2)                      # spawn and capture: every process run as the user
        for d in drops:
            body = d.split("\n\n")[0]
            self.assertLess(body.index("os.setuid(uid)"), body.index("no_new_privs()"), body)

    def test_an_inspection_without_the_groups_and_hostname_facts_is_refused(self):
        for key in ("groups", "hostname"):
            e = ArmEnv(self)
            insp = json.load(open(e.insp_path))
            del insp[key]
            json.dump(insp, open(e.insp_path, "w"))
            with self.assertRaises(armmod.ArmError) as cm:
                armmod.load_config(e.write())
            self.assertIn("regenerate", str(cm.exception))

    def test_services_plan_for_the_image_profile(self):
        plan, _ = plan_for()
        specs = tr.services_plan(plan)
        self.assertEqual([s["name"] for s in specs], ["service-env", "system_dbus", "dbus", "display", "login", "desktop", "xsettings", "mediakeys", "server"])
        by = {s["name"]: s for s in specs}
        self.assertEqual(by["display"]["argv"][:3], ["/usr/bin/Xvfb", ":0", "-screen"])
        self.assertEqual(by["display"]["argv"][4], "1920x1080x24")
        self.assertEqual(by["server"]["argv"], ["/usr/bin/python3", "/home/user/server/main.py"])   # the IMAGE's own launch line
        self.assertEqual(by["server"]["cwd"], "/home/user/server")
        self.assertEqual(by["server"]["wait"]["tcp"], ["127.0.0.1", 5000])
        # GNOME and the login are part of the workload: never tolerated dead; the settings daemons are logged when they exit
        self.assertEqual([s["name"] for s in specs if s["critical"]],
                         ["service-env", "system_dbus", "dbus", "display", "login", "desktop", "server"])
        self.assertEqual([(s["argv"], s["env"]) for s in specs if s.get("component")],
                         [(["/usr/libexec/gsd-xsettings"], "session"), (["/usr/libexec/gsd-media-keys"], "session")])
        self.assertEqual((by["desktop"]["env"], by["server"]["env"]), ("session", "service"))

    def test_gnome_gets_the_images_own_system_bus_first_run_as_root_with_its_socket_awaited(self):
        plan, _ = plan_for()
        sysb = next(s for s in tr.services_plan(plan) if s["name"] == "system_dbus")
        self.assertEqual(sysb["name"], "system_dbus")
        self.assertEqual(sysb["argv"], ["/usr/bin/dbus-daemon", "--system", "--nofork", "--nopidfile"])   # the image's binary
        self.assertEqual(sysb["user"], "root")
        self.assertEqual(sysb["mkdirs"], {"/run/dbus": 0o755})
        self.assertEqual(sysb["wait"], {"path": "/run/dbus/system_bus_socket"})
        self.assertTrue(sysb["critical"])
        self.assertEqual(sysb["env"], "service")                                  # what pid 1 gives a system service
        self.assertEqual(plan["inspection"]["system_bus"], {"system_conf": True, "messagebus_user": True, "machine_id": True})

    def test_gnome_x11_refuses_an_image_without_a_usable_system_bus_configuration(self):
        for kw in ({"system_bus": False},):
            plan, _ = plan_for(make_rootfs(tempfile.mkdtemp(), **kw))
            with self.assertRaises(tr.PlanError) as cm:
                tr.services_plan(plan)
            self.assertIn("system bus", str(cm.exception))
        plan, _ = plan_for(make_rootfs(tempfile.mkdtemp(), system_bus=False), profile="display_only")
        self.assertNotIn("system_dbus", [s["name"] for s in tr.services_plan(plan)])    # only the GNOME profile needs it


    @unittest.skipUnless(os.geteuid() == 0, "needs root to drop to another uid")
    def test_xorg_log_directory_is_writable_after_the_privilege_drop_unlike_a_root_owned_one(self):
        # the permission semantics behind the log path: a process that dropped to the image user opens its OWN logfile
        base = tmp(self)
        os.chmod(base, 0o755)                                                     # mkdtemp is 0700: the user must be able to traverse
        d = os.path.join(base, "root")
        os.makedirs(d)
        os.chmod(d, 0o755)
        uid = 65534

        def write_as_user(path):
            code = ("import os,sys\nos.setgroups([%d]); os.setgid(%d); os.setuid(%d)\n"
                    "open(sys.argv[1] + '/Xorg.log', 'w').write('x')\n" % (uid, uid, uid))
            return subprocess.run([sys.executable, "-c", code, path], capture_output=True, text=True)
        log_dir = os.path.join(d, "var/log/sharedos")
        os.makedirs(os.path.dirname(log_dir))
        os.chmod(os.path.dirname(log_dir), 0o755)
        for p in (d, os.path.join(d, "var"), os.path.dirname(log_dir)):
            os.chmod(p, 0o755)
        tr.Ops().prepare_dir(log_dir + "-rootowned", 0o755)                         # the previous behaviour
        self.assertNotEqual(write_as_user(log_dir + "-rootowned").returncode, 0)    # PermissionError: the bug
        tr.Ops().prepare_dir(log_dir, 0o755, uid, uid)                              # the new behaviour
        r = write_as_user(log_dir)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue(os.path.exists(os.path.join(log_dir, "Xorg.log")))
        # and the root-spawned tree can still create the per-service logs in that directory
        with open(os.path.join(log_dir, "system_dbus.log"), "ab") as f:
            f.write(b"x")

    def test_an_inspection_without_the_system_bus_facts_is_refused_with_a_regenerate_message(self):
        e = ArmEnv(self)
        insp = json.load(open(e.insp_path))
        del insp["system_bus"]
        json.dump(insp, open(e.insp_path, "w"))
        with self.assertRaises(armmod.ArmError) as cm:
            armmod.load_config(e.write())
        self.assertIn("regenerate", str(cm.exception))


    def test_display_only_profile_omits_the_shell_and_unknown_profiles_refuse(self):
        plan, _ = plan_for(profile="display_only")
        self.assertEqual([s["name"] for s in tr.services_plan(plan)], ["dbus", "display", "server"])
        plan["profile"] = "bogus"
        with self.assertRaises(tr.PlanError):
            tr.services_plan(plan)

    def test_plan_refuses_what_the_inspection_cannot_support(self):
        r = make_rootfs(tempfile.mkdtemp(), gnome=False)
        plan, _ = plan_for(r)
        with self.assertRaises(tr.PlanError) as cm:
            tr.services_plan(plan)
        self.assertIn("gnome-shell", str(cm.exception))
        r2 = make_rootfs(tempfile.mkdtemp(), xvfb=False)
        plan2, _ = plan_for(r2)
        with self.assertRaises(tr.PlanError) as cm2:
            tr.services_plan(plan2)
        self.assertIn("no headless X server", str(cm2.exception))
        plan3, _ = plan_for()
        plan3["inspection"]["ok_for_server"] = False
        with self.assertRaises(tr.PlanError):
            tr.services_plan(plan3)

    def test_xorg_dummy_gets_a_generated_config(self):
        plan, _ = plan_for(make_rootfs(tempfile.mkdtemp(), xvfb=False, xorg=True, dummy=True))
        disp = next(x for x in tr.services_plan(plan) if x["name"] == "display")
        self.assertEqual(disp["argv"][:2], ["/usr/bin/Xorg", ":0"])
        self.assertIn("/var/log/sharedos/Xorg.log", disp["argv"])      # in the upper layer, so it survives for review
        self.assertIn('Modes "1920x1080"', disp["write"]["/tmp/xorg-dummy.conf"])

    def test_user_env_has_the_session_variables_and_launcher_overrides(self):
        plan, _ = plan_for()
        env = tr.user_env(plan)
        for k, v in (("HOME", "/home/user"), ("USER", "user"), ("DISPLAY", ":0"), ("XDG_RUNTIME_DIR", "/run/user/1000"),
                     ("DBUS_SESSION_BUS_ADDRESS", "unix:path=/run/user/1000/bus"), ("LANG", "en_HK.UTF-8"),
                     ("LANGUAGE", "en_HK:en"), ("GDMSESSION", "ubuntu"), ("USERNAME", "user")):
            self.assertEqual(env[k], v)
        self.assertNotIn("FOO", env)                              # the server unit's Environment= is the server's, not the session's
        srv = dict(tr.service_env(plan), **tr.server_env_extra(plan))
        self.assertEqual((srv["DISPLAY"], srv["FOO"], srv["LANG"], srv["HOME"]), (":0", "bar", "en_HK.UTF-8", "/home/user"))
        self.assertEqual(srv["PATH"], tr.SYSTEMD_DEFAULT_PATH)
        for k in ("XDG_CURRENT_DESKTOP", "XDG_SESSION_TYPE", "GNOME_SHELL_SESSION_MODE", "DESKTOP_SESSION"):
            self.assertNotIn(k, srv)                              # a system unit is not in the login session

    def test_build_plan_falls_back_to_python3_main_when_the_image_has_no_launcher(self):
        plan, _ = plan_for(make_rootfs(tempfile.mkdtemp(), unit=False))
        self.assertEqual(plan["server"]["argv"], ["python3", "/home/user/server/main.py"])
        self.assertEqual(plan["server"]["cwd"], "/home/user/server")











# ------------------------------------------------------------------------- stage A (the tree) and stage B (the supervisor)
class StageOps:
    """Stage A system calls, recorded. fail_at=<op name> raises there. exec_supervisor returns (the real one never does)."""
    def __init__(self, fail_at=None, exc=OSError("boom")):
        self.events, self.fail_at, self.exc = [], fail_at, exc

    def _do(self, name, *a):
        self.events.append((name, *a))
        if self.fail_at == name:
            raise self.exc

    def run(self, argv):
        self._do("run", tuple(argv[:3]))

    def mkdir(self, p):
        self._do("mkdir", p)

    def touch(self, p):
        self._do("touch", p)

    def mknod(self, path, mode, major, minor, uid, gid):
        self._do("mknod", path, oct(mode), major, minor, uid, gid)

    def symlink(self, t, p):
        self._do("symlink", p)

    def lo_up(self):
        self._do("lo_up")

    def become_session_leader(self):
        self._do("setsid")
        return getattr(self, "ids", (1, 1, 1))

    def set_hostname(self, name):
        self._do("hostname", name)

    def preload(self):
        self._do("preload")

    def harden_runtime(self):
        self._do("harden")

    def apply_metadata_firewall(self):
        self._do("metadata")

    def prepare_dir(self, p, mode, uid=None, gid=None):
        self._do("prepare", p, oct(mode), uid)

    def strip_admin_groups(self, merged, user, groups):
        self._do("strip_admin_groups", merged, user, tuple(groups))

    def write_file(self, p, t):
        self._do("write", p)

    def supervisor_source(self):
        self._do("source")
        return tr.Ops().supervisor_source()

    def pivot_root(self, m):
        self._do("pivot", m)

    def redirect_stdio(self, path):
        self._do("redirect", path)

    def exec_supervisor(self, python, source, payload, marker):
        self.events.append(("exec", python, marker))
        self.payload, self.source = payload, source
        if self.fail_at == "exec_supervisor":
            raise self.exc


class BringUpTests(unittest.TestCase):
    """Stage A. The CRIU rule behind it (criu/mount.c, read at the pinned commit): the dumped root task's /proc/PID/root must be '/'
    of its mount namespace, which a chroot inside the host's mount namespace is not -> pivot_root and an image-side supervisor."""

    def test_order_is_mounts_hostname_preparation_then_pivot_then_stdio_then_exec(self):
        plan, _ = plan_for()
        ops = StageOps()
        tr.bring_up(plan, ops, "M1", getpid=lambda: 1)
        kinds = [e[0] for e in ops.events]
        # own session, then the egress block, then lock down privileges, before source is read / anything mounts. The firewall comes
        # BEFORE the bounding-set drop: nft is exec'd, and an exec after the drop has no CAP_NET_ADMIN (seen live: EPERM from nft).
        self.assertEqual(kinds[:4], ["setsid", "metadata", "harden", "source"])
        self.assertLess(kinds.index("harden"), kinds.index("run"))             # no_new_privs+caps+seccomp before the first mount
        self.assertLess(kinds.index("metadata"), kinds.index("pivot"))
        self.assertLess(kinds.index("run"), kinds.index("lo_up"))
        self.assertLess(kinds.index("lo_up"), kinds.index("hostname"))
        self.assertLess(kinds.index("hostname"), kinds.index("preload"))
        self.assertLess(kinds.index("preload"), kinds.index("pivot"))
        self.assertLess(max(i for i, k in enumerate(kinds) if k in ("prepare", "write", "mknod")), kinds.index("pivot"))
        self.assertEqual(kinds[-3:], ["pivot", "redirect", "exec"])            # nothing runs between the pivot and the exec
        self.assertNotIn("chroot", kinds)
        self.assertEqual(ops.events[-3][1], plan["merged"])                    # the overlay becomes the namespace root
        self.assertEqual(ops.events[-2], ("redirect", "/var/log/sharedos/tree.log"))
        self.assertEqual(ops.events[-1], ("exec", "/usr/bin/python3", "M1"))   # the IMAGE's python, never the host's

    def test_the_init_becomes_its_own_session_leader_before_any_mount_or_service(self):
        plan, _ = plan_for()
        ops = StageOps()
        tr.bring_up(plan, ops, "M", getpid=lambda: 1)
        kinds = [e[0] for e in ops.events]
        self.assertEqual(kinds[0], "setsid")                                     # first thing, before the first mount
        self.assertLess(kinds.index("setsid"), kinds.index("run"))
        self.assertLess(kinds.index("setsid"), kinds.index("exec"))
        for ids in ((1, 0, 0), (1, 1, 0), (1, 0, 1), (1, 1510, 1510)):          # sid/pgid of a leader outside the namespace
            ops2 = StageOps()
            ops2.ids = ids
            with self.assertRaises(tr.PlanError) as cm:
                tr.bring_up(plan, ops2, "M", getpid=lambda: 1)
            self.assertIn("leader of its own session", str(cm.exception))
            self.assertEqual([e[0] for e in ops2.events], ["setsid"])           # nothing was mounted
        ops3 = StageOps("setsid", OSError("EPERM: already a process group leader"))
        d = tmp(self)
        pf = os.path.join(d, "p.json")
        json.dump(plan, open(pf, "w"))
        with mock.patch.object(tr, "install_term_handler", lambda: None), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(tr.main(["--marker", "M", "--plan", pf], ops=ops3), 5)
        self.assertNotIn("run", [e[0] for e in ops3.events])

    @unittest.skipUnless(os.geteuid() == 0 and shutil.which("unshare") and shutil.which("setsid"), "needs root, unshare and setsid")
    def test_real_namespace_init_has_a_foreign_session_until_it_calls_setsid(self):
        # the launcher's shape: `setsid` first, then `unshare --pid --fork`; the failing CRIU log said sid=0 pgid=0 pid=1
        code = (f"import os, sys\nsys.path.insert(0, {PKG_ROOT!r})\nfrom sharedos_rootfs import tree\n"
                "before = (os.getpid(), os.getsid(0), os.getpgid(0))\nafter = tree.Ops().become_session_leader()\n"
                "print(before, after, flush=True)\n")
        r = subprocess.run(["setsid", "-w", "unshare", "--pid", "--fork", sys.executable, "-c", code], capture_output=True,
                           text=True, timeout=60)
        if r.returncode != 0 and ("ermission" in r.stderr or "not permitted" in r.stderr):
            self.skipTest("this sandbox cannot create a pid namespace: " + r.stderr[-200:])
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(r.stdout.strip(), "(1, 0, 0) (1, 1, 1)")             # before: leader outside the ns; after: its own session

    def test_preparation_goes_through_the_overlay_path_before_the_pivot_with_exact_owners(self):
        plan, _ = plan_for(make_rootfs(tempfile.mkdtemp(), xvfb=False, xorg=True, dummy=True))
        ops = StageOps()
        tr.bring_up(plan, ops, "M", getpid=lambda: 1)
        m = plan["merged"]
        self.assertIn(("prepare", f"{m}/run/user/1000", "0o700", 1000), ops.events)
        self.assertIn(("prepare", f"{m}/tmp/.X11-unix", "0o1777", None), ops.events)
        self.assertIn(("prepare", f"{m}/var/log/sharedos", "0o755", 1000), ops.events)    # Xorg writes its own log after the drop
        self.assertIn(("prepare", f"{m}/run/dbus", "0o755", None), ops.events)
        self.assertIn(("write", f"{m}/tmp/xorg-dummy.conf"), ops.events)
        self.assertLess(ops.events.index(("write", f"{m}/tmp/xorg-dummy.conf")), ops.events.index(("pivot", m)))

    def test_the_payload_carries_specs_env_user_groups_and_no_preparation_work(self):
        plan, _ = plan_for()
        ops = StageOps()
        tr.bring_up(plan, ops, "M", getpid=lambda: 1)
        pl = ops.payload
        self.assertEqual([s["name"] for s in pl["specs"]], ["service-env", "system_dbus", "dbus", "display", "login", "desktop", "xsettings", "mediakeys", "server"])
        self.assertEqual(set(pl["envs"]), {"session_base", "service_base"})
        self.assertTrue(all("mkdirs" not in s and "write" not in s for s in pl["specs"]))      # done in stage A
        self.assertEqual(pl["user"], {"uid": 1000, "gid": 1000, "groups": [24]})                # the image's own groups, minus sudo
        self.assertEqual(pl["env"]["XDG_CURRENT_DESKTOP"], "ubuntu:GNOME")
        self.assertEqual(pl["log_dir"], "/var/log/sharedos")
        json.dumps(pl)                                                                          # it travels as JSON on stdin
        self.assertTrue(ops.source.startswith("# drumsbench-sharedos-supervisor"))

    def test_image_hostname_is_set_before_the_pivot_and_mknod_nodes_exist_before_it(self):
        plan, _ = plan_for()
        ops = StageOps()
        tr.bring_up(plan, ops, "M", getpid=lambda: 1)
        kinds = [e[0] for e in ops.events]
        self.assertIn(("hostname", "osworld-vm"), ops.events)
        self.assertLess(kinds.index("hostname"), kinds.index("pivot"))
        self.assertEqual(kinds.count("mknod"), 6)
        self.assertLess(max(i for i, e in enumerate(ops.events) if e[0] == "mknod"), kinds.index("pivot"))
        plan["hostname"] = None                                                 # image without /etc/hostname: nothing is set
        ops2 = StageOps()
        tr.bring_up(plan, ops2, "M", getpid=lambda: 1)
        self.assertNotIn("hostname", [e[0] for e in ops2.events])

    def test_refuses_outside_a_private_pid_namespace_before_any_mount(self):
        plan, _ = plan_for()
        ops = StageOps()
        with self.assertRaises(tr.PlanError):
            tr.bring_up(plan, ops, "M", getpid=lambda: 4242)
        self.assertEqual(ops.events, [])

    def test_a_plan_the_inspection_cannot_support_fails_before_any_mount(self):
        plan, _ = plan_for(make_rootfs(tempfile.mkdtemp(), xvfb=False))
        ops = StageOps()
        with self.assertRaises(tr.PlanError):
            tr.bring_up(plan, ops, "M", getpid=lambda: 1)
        self.assertEqual(ops.events, [])

    def test_unsafe_identity_facts_are_refused_before_anything_is_mounted(self):
        for mutate, text in ((lambda p: p["user"].__setitem__("groups", [0]), "supplementary gid"),
                             (lambda p: p["user"].__setitem__("groups", [1000]), "primary gid"),
                             (lambda p: p["user"].__setitem__("groups", ["27"]), "supplementary gid"),
                             (lambda p: p.__setitem__("tty_gid", None), "tty"),
                             (lambda p: p.__setitem__("hostname", "bad host;rm"), "host name")):
            plan, _ = plan_for()
            mutate(plan)
            ops = StageOps()
            with self.assertRaises(tr.PlanError) as cm:
                tr.bring_up(plan, ops, "M", getpid=lambda: 1)
            self.assertIn(text, str(cm.exception))
            self.assertEqual(ops.events, [])

    def test_any_failure_before_the_exec_exits_5_with_the_traceback_and_never_execs(self):
        plan, _ = plan_for()
        d = tmp(self)
        pf = os.path.join(d, "p.json")
        json.dump(plan, open(pf, "w"))
        for fail_at, exc in (("mknod", OSError("EPERM")), ("pivot", OSError("pivot_root failed: EINVAL")),
                             ("preload", LookupError("unknown encoding: idna")), ("redirect", ValueError("x"))):
            ops = StageOps(fail_at, exc)
            err = io.StringIO()
            with mock.patch.object(tr, "install_term_handler", lambda: None), contextlib.redirect_stderr(err):
                rc = tr.main(["--marker", "M", "--plan", pf], ops=ops)
            self.assertEqual(rc, 5, fail_at)
            self.assertIn("slot failed", err.getvalue())
            self.assertIn("Traceback", err.getvalue())
            self.assertNotIn("exec", [e[0] for e in ops.events], fail_at)
        ops = StageOps("exec_supervisor", OSError("exec failed"))             # the exec itself failing is also a clean failure
        with mock.patch.object(tr, "install_term_handler", lambda: None), contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(tr.main(["--marker", "M", "--plan", pf], ops=ops), 5)

    def test_host_modules_are_preloaded_before_the_pivot(self):
        code = ("import sys\nsys.modules.pop('encodings.idna', None)\nfrom sharedos_rootfs import tree\n"
                "tree.preload_host_modules()\nassert 'encodings.idna' in sys.modules\nassert 'ctypes' in sys.modules\n"
                "assert 'platform' in sys.modules\n")
        r = subprocess.run([sys.executable, "-c", code], cwd=PKG_ROOT, capture_output=True, text=True, timeout=30)
        self.assertEqual(r.returncode, 0, r.stderr)

    def test_pivot_root_into_follows_the_documented_sequence_and_fails_closed(self):
        calls, errno_box = [], {}

        class FakeLibc:
            def __init__(self, fail=None):
                self.fail = fail

            def syscall(self, nr, a, b):
                calls.append(("pivot_root", nr, a, b))
                return -1 if self.fail == "pivot" else 0

            def mount(self, src, tgt, fs, flags, data):
                calls.append(("mount", tgt, flags))
                return -1 if self.fail == "mount" else 0

            def umount2(self, tgt, flags):
                calls.append(("umount2", tgt, flags))
                return -1 if self.fail == "umount" else 0

        class FakeOs:
            O_RDONLY, O_DIRECTORY = os.O_RDONLY, os.O_DIRECTORY

            def __init__(self):
                self.n = 10

            def open(self, p, f):
                self.n += 1
                calls.append(("open", p))
                return self.n

            def fchdir(self, fd):
                calls.append(("fchdir", fd))

            def chdir(self, p):
                calls.append(("chdir", p))

            def close(self, fd):
                calls.append(("close", fd))
        tr.pivot_root_into("/new", FakeLibc(), FakeOs())
        names = [c[0] for c in calls]
        self.assertEqual(names, ["open", "open", "fchdir", "pivot_root", "fchdir", "mount", "umount2", "chdir", "close", "close"])
        self.assertEqual(calls[3][2:], (b".", b"."))                            # pivot_root(".", "."): no put_old dir is created
        self.assertEqual(calls[5][1:], (b".", tr.MS_REC | tr.MS_PRIVATE))      # the old root cannot propagate the detach to the host
        self.assertEqual(calls[6][1:], (b".", tr.MNT_DETACH))
        self.assertEqual(calls[2][1], 12)                                      # first the NEW root, then back to the old one
        self.assertEqual(calls[4][1], 11)
        for fail in ("pivot", "mount", "umount"):
            del calls[:]
            with self.assertRaises(OSError):
                tr.pivot_root_into("/new", FakeLibc(fail), FakeOs())
            self.assertEqual(sum(1 for c in calls if c[0] == "close"), 2, fail)   # fds are never leaked

    @unittest.skipUnless(os.geteuid() == 0 and shutil.which("unshare"), "needs root and unshare")
    def test_real_pivot_makes_proc_pid_root_slash_which_a_chroot_does_not(self):
        # the exact check CRIU makes (criu/mount.c: readlink of /proc/PID/root must be "/"), proven with a real kernel
        def run(use_pivot):
            d = tmp(self)
            os.chmod(d, 0o755)
            code = f"""
import os, subprocess, sys, time
sys.path.insert(0, {PKG_ROOT!r})
from sharedos_rootfs import tree
nr = {d!r} + "/newroot"
os.makedirs(nr)
subprocess.run(["mount", "-t", "tmpfs", "tmpfs", nr], check=True)
os.makedirs(nr + "/proc")
subprocess.run(["mount", "-t", "proc", "proc", nr + "/proc"], check=True)
open(nr + "/inside", "w").write("x")
tree.preload_host_modules()
if {use_pivot!r}:
    tree.pivot_root_into(nr)
else:
    os.chroot(nr); os.chdir("/")
mi = open("/proc/self/mountinfo").read().splitlines()
print("READY", sorted(os.listdir("/")), [l.split()[4] for l in mi], flush=True)
time.sleep(60)
"""
            p = subprocess.Popen(["unshare", "--mount", "--propagation", "private", sys.executable, "-c", code],
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                line = p.stdout.readline()
                if not line.startswith("READY"):
                    err = p.stderr.read() if p.poll() is not None else ""
                    if "ermission" in err or "not permitted" in err or "mount:" in err:
                        self.skipTest("this sandbox cannot create mounts: " + err[-200:])
                    self.fail(f"child did not become ready: {line!r} {err[-400:]}")
                return line.strip(), os.readlink(f"/proc/{p.pid}/root")
            finally:
                p.kill()
                p.wait()
        line, root = run(True)
        self.assertEqual(root, "/")                                                # CRIU's requirement holds after pivot_root
        self.assertIn("['inside', 'proc']", line)                                  # only the slot's tree is visible
        self.assertIn("['/', '/proc']", line)                                      # the host's mounts are gone from the namespace
        _, chroot_root = run(False)
        self.assertNotEqual(chroot_root, "/")                                      # the old behaviour reproduces CRIU's error
        self.assertIn("newroot", chroot_root)


SV_SPEC = {"name": "x", "argv": ["/usr/bin/x"], "wait": {"alive_s": 0}, "critical": True}


class SvOps:
    """Supervisor system calls, recorded."""
    def __init__(self, fail_wait=None, fail_noncritical=None):
        self.events, self.fail_wait, self.fail_nc = [], fail_wait, fail_noncritical
        self.spawned = []
        self.env_of = {}

    def prepare_dir(self, p, mode, uid=None, gid=None):
        self.events.append(("prepare", p, oct(mode)))

    def write_file(self, p, t):
        self.events.append(("write", p))

    def spawn(self, spec, env, uid, gid, groups=()):
        class P:
            pid = 100 + len(self.spawned)
            returncode = None
            terminated = killed = False

            def poll(s):
                return None if not s.terminated and not s.killed else 0

            def terminate(s):
                s.terminated = True

            def kill(s):
                s.killed = True

            def wait(s):
                return 0
        p = P()
        p.spec = spec
        self.env_of[spec["name"]] = env
        self.spawned.append(p)
        self.events.append(("spawn", spec["name"], uid, gid, tuple(groups)))
        return p

    def event(self, msg):
        self.events.append(("event", msg))

    def capture(self, name, argv, env, uid, gid, groups=(), timeout=60, stdin=b""):
        self.events.append(("capture", name, tuple(argv)))
        if argv[0].endswith("gnome-keyring-daemon") and "--start" in argv:
            return 0, b"SSH_AUTH_SOCK=/run/user/1000/keyring/ssh\n"
        if len(argv) == 2 and argv[0].endswith("Xsession"):
            self.xsession_env = dict(env, FROM_XSESSION="1")
        return 0, b""

    def write_user_exec(self, path, text, uid, gid):
        self.events.append(("write_user_exec", path))

    def remove(self, path):
        pass

    def read_and_remove(self, path):
        return b"\0".join(f"{k}={v}".encode() for k, v in self.xsession_env.items())

    def write_json(self, name, obj):
        self.events.append(("write_json", name))

    def tail(self, name, n=600):
        return f"{name}: Gio.IOErrorEnum Could not connect" if name in (self.fail_wait, self.fail_nc) else ""

    def wait_for(self, w, proc):
        name = proc.spec["name"]
        self.events.append(("wait", name))
        if name in (self.fail_wait, self.fail_nc):
            raise RuntimeError("not ready")


def sv_payload(plan=None, **kw):
    plan = plan or plan_for()[0]
    specs = tr.services_plan(plan)
    pl = {"specs": [{k: v for k, v in s.items() if k not in ("mkdirs", "write")} for s in specs], "env": tr.user_env(plan),
          "envs": tr.payload_envs(plan),
          "user": {"uid": 1000, "gid": 1000, "groups": [27, 24]}, "log_dir": "/var/log/sharedos"}
    pl.update(kw)
    return pl


class SupervisorTests(unittest.TestCase):
    def test_the_supervisor_is_stdlib_only_python38_syntax_and_imports_nothing_from_the_package_or_host(self):
        import ast
        with open(os.path.join(PKG_ROOT, "sharedos_rootfs", "supervisor.py")) as f:
            src = f.read()
        self.assertTrue(src.startswith("# drumsbench-sharedos-supervisor"))
        tree_ = ast.parse(src, feature_version=(3, 8))                          # the image's python is 3.10
        stdlib = {"ctypes", "json", "os", "re", "signal", "socket", "subprocess", "sys", "time", "traceback"}
        mods = set()
        for n in ast.walk(tree_):
            if isinstance(n, ast.Import):
                mods |= {a.name.split(".")[0] for a in n.names}
            elif isinstance(n, ast.ImportFrom):
                mods.add((n.module or "").split(".")[0])
                self.assertEqual(n.level, 0)
        self.assertLessEqual(mods, stdlib)

    def test_services_start_in_plan_order_with_the_image_groups_and_each_is_awaited(self):
        ops = SvOps()
        procs = sv.start_services(sv_payload(), ops)
        self.assertEqual([e[1] for e in ops.events if e[0] == "spawn"], ["system_dbus", "dbus", "display", "desktop", "xsettings", "mediakeys", "server"])
        self.assertEqual([e[1] for e in ops.events if e[0] == "spawn"][0:1], ["system_dbus"])
        self.assertEqual({e[4] for e in ops.events if e[0] == "spawn"}, {(27, 24)})
        for e in [e for e in ops.events if e[0] == "spawn"]:
            self.assertLess(ops.events.index(e), ops.events.index(("wait", e[1])))
        self.assertEqual([(p.service, p.critical) for p in procs],
                         [("system_dbus", True), ("dbus", True), ("display", True), ("desktop", True), ("xsettings", False),
                          ("mediakeys", False), ("server", True)])
        # the login ran after the display and before the desktop, and the desktop got its environment
        names = [e[1] for e in ops.events if e[0] in ("spawn", "capture")]
        self.assertLess(names.index("display"), names.index("login"))
        self.assertLess(names.index("login"), names.index("desktop"))
        self.assertEqual(ops.env_of["desktop"]["FROM_XSESSION"], "1")
        self.assertNotIn("FROM_XSESSION", ops.env_of["server"])

    def test_a_critical_service_failure_shuts_down_everything_started_and_nothing_later_starts(self):
        ops = SvOps(fail_wait="display")
        with self.assertRaises(RuntimeError) as cm:
            sv.start_services(sv_payload(), ops)
        self.assertIn("service display failed", str(cm.exception))
        self.assertTrue(all(p.terminated or p.killed for p in ops.spawned))
        self.assertNotIn("server", [e[1] for e in ops.events if e[0] == "spawn"])

    def test_a_dead_gnome_desktop_fails_the_slot_with_the_log_tail(self):
        ops = SvOps(fail_noncritical="desktop")
        with self.assertRaises(RuntimeError) as cm:
            sv.start_services(sv_payload(), ops)
        self.assertIn("service desktop failed", str(cm.exception))
        self.assertIn("Gio.IOErrorEnum", str(cm.exception))
        self.assertTrue(any(e[0] == "event" and "NOT READY desktop" in e[1] for e in ops.events))

    def test_any_exception_while_starting_stops_what_was_started(self):
        ops = SvOps()
        real = ops.wait_for

        def lookup(w, proc):
            real(w, proc)
            if proc.spec["name"] == "display":
                raise LookupError("unknown encoding: idna")                   # not a RuntimeError
        ops.wait_for = lookup
        with self.assertRaises(LookupError):
            sv.start_services(sv_payload(), ops)
        self.assertTrue(all(p.terminated or p.killed for p in ops.spawned))

    def test_display_only_profile_has_no_desktop(self):
        plan, _ = plan_for(profile="display_only")
        procs = sv.start_services(sv_payload(plan), SvOps())
        self.assertEqual([p.service for p in procs], ["dbus", "display", "server"])

    class P:
        def __init__(self, pid, service, critical):
            self.pid, self.service, self.critical, self.rc = pid, service, critical, None

        def poll(self):
            return self.rc

    class Ops:
        def __init__(self):
            self.reaped = []

        def reap_orphans(self, known):
            self.reaped.append(set(known))
            return []

    def procs(self):
        return [self.P(1, "dbus", True), self.P(2, "display", True), self.P(3, "desktop", False), self.P(4, "server", True)]

    def test_healthy_slot_keeps_supervising_and_reaps_orphans_without_touching_services(self):
        ps, ops = self.procs(), self.Ops()
        sv.supervise(ps, ops, sleep=lambda s: None, ticks=3)
        self.assertEqual(ops.reaped, [{1, 2, 3, 4}] * 3)

    def test_each_critical_service_that_dies_fails_the_slot(self):
        for i, name in ((0, "dbus"), (1, "display"), (3, "server")):
            ps = self.procs()
            ps[i].rc = 1
            with self.assertRaises(sv.CriticalServiceDied) as cm:
                sv.supervise(ps, self.Ops(), sleep=lambda s: None, ticks=5)
            self.assertIn(f"critical service {name} exited rc=1", str(cm.exception))
        ps = self.procs()
        ps[3].rc = 0
        with self.assertRaises(sv.CriticalServiceDied):                         # a clean exit of a critical service is a failure
            sv.supervise(ps, self.Ops(), sleep=lambda s: None, ticks=1)

    def test_noncritical_exit_is_tolerated_and_logged_once(self):
        ps, ops = self.procs(), self.Ops()
        ps[2].rc = 1
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            sv.supervise(ps, ops, sleep=lambda s: None, ticks=3)
        self.assertEqual(err.getvalue().count("non-critical service desktop exited rc=1"), 1)

    def test_reap_orphans_reaps_a_non_service_child_and_leaves_a_service_for_poll(self):
        service = subprocess.Popen([sys.executable, "-c", "pass"])
        orphan = subprocess.Popen([sys.executable, "-c", "pass"])
        time.sleep(1.0)
        reaped = sv.Ops().reap_orphans({service.pid})
        self.assertIn(orphan.pid, reaped)
        self.assertNotIn(service.pid, reaped)
        self.assertEqual(service.poll(), 0)                                      # the exit was not lost

    def test_shutdown_is_orderly_then_forceful_and_reaps(self):
        good = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        stubborn = subprocess.Popen([sys.executable, "-c",
                                     "import signal, time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)"])
        time.sleep(0.6)
        t0 = time.monotonic()
        rep = sv.shutdown([good, stubborn], grace_s=0.5)
        self.assertGreaterEqual(time.monotonic() - t0, 0.5)
        self.assertEqual(rep["sigkilled"], [stubborn.pid])
        self.assertEqual(rep["sigterm_exits"], [good.pid])
        self.assertIsNotNone(stubborn.returncode)

    def test_tcp_readiness_needs_no_resolver_or_text_codec_the_post_chroot_failure(self):
        srv = socket.socket()
        srv.bind(("127.0.0.1", 0))
        srv.listen(1)
        self.addCleanup(srv.close)
        code = ("import sys, socket\nsys.modules['encodings.idna'] = None\n"
                "def boom(*a, **k): raise LookupError('unknown encoding: idna')\n"
                "socket.getaddrinfo = boom\nsocket.create_connection = boom\n"
                "import importlib.util\nspec = importlib.util.spec_from_file_location('sv', %r)\n"
                "sv = importlib.util.module_from_spec(spec); spec.loader.exec_module(sv)\n"
                "assert sv.tcp_probe('127.0.0.1', %d) is True\nassert sv.tcp_probe('127.0.0.1', 1) is False\n"
                "try:\n    sv.tcp_probe('localhost', 80)\nexcept ValueError:\n    pass\nelse:\n    sys.exit(3)\n"
                % (os.path.join(PKG_ROOT, "sharedos_rootfs", "supervisor.py"), srv.getsockname()[1]))
        r = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, timeout=30)
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(os.path.join(PKG_ROOT, "sharedos_rootfs", "supervisor.py")) as f:
            code_src = "\n".join(l for l in f.read().splitlines() if not l.lstrip().startswith("#"))
        self.assertNotIn("create_connection", code_src)
        proc = mock.Mock()
        proc.poll.return_value = None
        sv.Ops().wait_for({"tcp": ["127.0.0.1", srv.getsockname()[1]], "timeout_s": 5}, proc)

    def test_spawn_drops_privileges_for_user_services_but_not_for_the_system_bus(self):
        seen = []

        class FakePopen:
            def __init__(self, argv, **kw):
                seen.append((argv, kw))
        d = tmp(self)
        with mock.patch.object(sv.subprocess, "Popen", FakePopen):
            env = {"HOME": "/home/user", "DISPLAY": ":0"}
            sv.Ops(d).spawn({"name": "dbus", "argv": ["/usr/bin/x"]}, env, 1000, 1000)
            sv.Ops(d).spawn({"name": "system_dbus", "argv": ["/usr/bin/y"], "user": "root"}, env, 1000, 1000)
        self.assertTrue(callable(seen[0][1]["preexec_fn"]))
        self.assertIsNone(seen[1][1]["preexec_fn"])
        self.assertEqual(seen[1][1]["env"]["HOME"], "/root")
        self.assertEqual(seen[0][1]["env"]["HOME"], "/home/user")

    def test_a_missing_binary_is_a_named_failure(self):
        d = tmp(self)
        with self.assertRaises(RuntimeError) as cm:
            sv.Ops(d).spawn({"name": "desktop", "argv": ["/nonexistent/gnome-shell"], "user": "root"}, {"HOME": "/"}, 1, 1)
        self.assertIn("cannot start desktop", str(cm.exception))

    @unittest.skipUnless(os.geteuid() == 0, "needs root to drop privileges")
    def test_spawn_applies_exactly_the_image_groups_not_the_hosts(self):
        d = tmp(self)
        os.chmod(d, 0o755)
        code = "import os; print(os.getuid(), os.getgid(), sorted(os.getgroups()))"
        env = {"HOME": d}
        sv.Ops(d).spawn({"name": "with", "argv": [sys.executable, "-c", code], "cwd": d}, env, 65534, 65534, (27, 24))
        sv.Ops(d).spawn({"name": "without", "argv": [sys.executable, "-c", code], "cwd": d}, env, 65534, 65534)
        deadline = time.monotonic() + 15
        out = {}
        while time.monotonic() < deadline and len(out) < 2:
            for n in ("with", "without"):
                txt = open(os.path.join(d, f"{n}.log")).read().strip()
                if txt:
                    out[n] = txt
            time.sleep(0.1)
        self.assertEqual(out["with"], "65534 65534 [24, 27, 65534]")        # the image's sudo + cdrom, plus the primary gid only
        self.assertEqual(out["without"], "65534 65534 [65534]")            # the previous behaviour: memberships lost
        with open(os.path.join(PKG_ROOT, "sharedos_rootfs", "supervisor.py")) as f:
            code_src = "\n".join(l for l in f.read().splitlines() if not l.lstrip().startswith("#"))
        self.assertNotIn("initgroups", code_src)                             # never the host's group database

    def test_main_exit_codes_7_for_a_dead_critical_service_5_for_any_other_failure_and_always_a_shutdown(self):
        shut = []
        base = dict(install=mock.patch.object(sv, "install_term_handler", lambda: None),
                    shut=mock.patch.object(sv, "shutdown", lambda ps: shut.append(ps) or {}))
        payload = json.dumps(sv_payload())
        died = self.procs()
        died[3].rc = 3
        with base["install"], base["shut"], mock.patch.object(sv, "start_services", lambda p, o: died), \
                contextlib.redirect_stderr(io.StringIO()) as err:
            self.assertEqual(sv.main(["--marker", "M"], stdin=io.StringIO(payload), ops=SvOps()), 7)
        self.assertIn("critical service server exited rc=3", err.getvalue())
        self.assertEqual(shut, [died])
        for exc in (LookupError("unknown encoding: idna"), ValueError("x")):
            del shut[:]
            with base["install"], base["shut"], mock.patch.object(sv, "start_services", mock.Mock(side_effect=exc)), \
                    contextlib.redirect_stderr(io.StringIO()) as err:
                self.assertEqual(sv.main(["--marker", "M"], stdin=io.StringIO(payload), ops=SvOps()), 5)
            self.assertIn("Traceback", err.getvalue())
            self.assertEqual(shut, [[]])
        with base["install"], mock.patch.object(sv, "start_services", mock.Mock(side_effect=ValueError("x"))), \
                mock.patch.object(sv, "shutdown", mock.Mock(side_effect=OSError("boom"))), \
                contextlib.redirect_stderr(io.StringIO()) as e2:
            self.assertEqual(sv.main(["--marker", "M"], stdin=io.StringIO(payload), ops=SvOps()), 5)    # never masks the failure
        self.assertIn("shutdown failed", e2.getvalue())
        with base["install"], base["shut"], contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(sv.main(["--marker", "M"], stdin=io.StringIO("not json"), ops=SvOps()), 5)

    @unittest.skipUnless(os.geteuid() == 0, "needs root for the privilege drop")
    def test_the_supervisor_runs_as_dash_c_text_with_the_plan_on_stdin_and_is_found_by_marker(self):
        # exactly how stage A execs it (minus the image's python): no package import, plan on stdin, marker visible in argv
        d = tmp(self)
        os.chmod(d, 0o755)
        logs = os.path.join(d, "logs")
        marker = "drumsbench-test-marker-" + os.path.basename(d)
        payload = {"specs": [{"name": "sleeper", "argv": [sys.executable, "-c", "import time; time.sleep(60)"], "cwd": d,
                              "wait": {"alive_s": 0.3}, "critical": True}],
                   "env": {"HOME": d, "PATH": "/usr/bin:/bin"}, "user": {"uid": 65534, "gid": 65534, "groups": []}, "log_dir": logs}
        with open(os.path.join(PKG_ROOT, "sharedos_rootfs", "supervisor.py")) as f:
            src = f.read()
        env = {k: v for k, v in os.environ.items() if k != "PYTHONPATH"}
        p = subprocess.Popen([sys.executable, "-c", src, "-m", "sharedos_rootfs.tree", "--marker", marker], cwd=d, env=env,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        p.stdin.write(json.dumps(payload))
        p.stdin.close()
        p.stdin = None            # communicate() must not flush a closed pipe (Python 3.11)
        events = os.path.join(logs, "tree-events.log")
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline and not (os.path.exists(events) and "ready sleeper" in open(events).read()):
            time.sleep(0.1)
        self.assertIn("ready sleeper", open(events).read())
        argv = open(f"/proc/{p.pid}/cmdline", "rb").read().split(b"\0")
        argv = [a.decode() for a in argv if a]
        from sharedos_spike.runtime import tree_argv_matches
        self.assertTrue(tree_argv_matches(argv, marker, module="sharedos_rootfs.tree"))   # the arm's identity check still works
        p.send_signal(signal.SIGTERM)
        out, err = p.communicate(timeout=30)
        self.assertEqual(p.returncode, 0, err)
        self.assertIn("shutdown:", out)
        self.assertNotIn("Traceback", err)


class TreeSessionTests(unittest.TestCase):
    def test_session_variables_come_only_from_the_images_own_session_files(self):
        r = make_rootfs(tmp(self))
        insp = ri.inspect_rootfs(r)
        ses = insp["session"]
        self.assertEqual(ses["name"], "ubuntu")
        self.assertEqual(ses["desktop_names"], "ubuntu:GNOME")
        self.assertEqual(ses["exec_env"], {"GNOME_SHELL_SESSION_MODE": "ubuntu"})
        self.assertEqual(ses["file"], "/usr/share/xsessions/ubuntu.desktop")
        env = tr.session_env(ses)
        self.assertEqual(env["XDG_CURRENT_DESKTOP"], "ubuntu:GNOME")             # what DING's GLib.getenv() needs
        self.assertEqual((env["XDG_SESSION_DESKTOP"], env["DESKTOP_SESSION"]), ("ubuntu", "ubuntu"))
        self.assertEqual(env["GNOME_SHELL_SESSION_MODE"], "ubuntu")
        self.assertEqual(env["XDG_CONFIG_DIRS"], "/etc/xdg/xdg-ubuntu:/etc/xdg")   # a literal from the image's gdm env.d
        self.assertNotIn("PATH", env)                                              # unsafe names are never taken
        self.assertNotIn("XDG_X", env)                                             # nor values needing shell expansion
        plan, _ = plan_for(r)
        uenv = tr.user_env(plan)
        self.assertEqual(uenv["XDG_CURRENT_DESKTOP"], "ubuntu:GNOME")
        self.assertEqual(uenv["PATH"], "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/snap/bin")  # pam_env
        self.assertEqual(uenv["XDG_SESSION_TYPE"], "x11")

    def test_an_image_without_a_gnome_session_file_has_none_invented_and_gnome_x11_refuses(self):
        r = make_rootfs(tmp(self))
        os.remove(os.path.join(r, "usr/share/xsessions/ubuntu.desktop"))
        self.assertIsNone(ri.inspect_rootfs(r)["session"])
        plan, _ = plan_for(r)
        with self.assertRaises(tr.PlanError) as cm:
            tr.services_plan(plan)
        self.assertIn("XDG_CURRENT_DESKTOP", str(cm.exception))
        self.assertNotIn("XDG_CURRENT_DESKTOP", tr.user_env(plan))                 # not guessed either
        plan2, _ = plan_for(r, profile="display_only")
        tr.services_plan(plan2)                                                    # no GNOME, so no session needed

    def test_the_old_tree_log_is_kept_not_appended_to_on_the_next_provision(self):
        e = ArmEnv(self)
        a, w, ex = e.arm()
        p = L.slot_paths(e.state, 0)
        os.makedirs(p["dir"])
        open(p["tree_log"], "w").write("OLD TRACEBACK")
        a.provision(0)
        rotated = [n for n in os.listdir(p["dir"]) if n.startswith("tree.log.")]
        self.assertEqual(len(rotated), 1)
        self.assertEqual(open(os.path.join(p["dir"], rotated[0])).read(), "OLD TRACEBACK")
        self.assertFalse(os.path.exists(p["tree_log"]))                            # the fake launch wrote nothing new

    def test_an_inspection_without_the_session_facts_is_refused_too(self):
        e = ArmEnv(self)
        insp = json.load(open(e.insp_path))
        del insp["session"]
        json.dump(insp, open(e.insp_path, "w"))
        with self.assertRaises(armmod.ArmError) as cm:
            armmod.load_config(e.write())
        self.assertIn("regenerate", str(cm.exception))



# ------------------------------------------------------------------------------------------ proxy
class ProxyTests(unittest.TestCase):
    def backend(self, handler):
        srv = socket.socket()
        srv.bind(("127.0.0.1", 0))
        srv.listen(8)

        def serve():
            while True:
                try:
                    c, _ = srv.accept()
                except OSError:
                    return
                threading.Thread(target=handler, args=(c,), daemon=True).start()
        threading.Thread(target=serve, daemon=True).start()
        self.addCleanup(srv.close)
        return srv.getsockname()[1]

    @staticmethod
    def echo(c):
        with c:
            while True:
                d = c.recv(4096)
                if not d:
                    return
                c.sendall(d.upper())

    def fwd(self, connector):
        f = px.Forwarder(0, connector)
        threading.Thread(target=f.serve_forever, daemon=True).start()
        self.addCleanup(f.close)
        return f

    def test_bytes_flow_both_ways_including_large_payloads(self):
        bp = self.backend(self.echo)
        f = self.fwd(lambda: socket.create_connection(("127.0.0.1", bp)))
        c = socket.create_connection(("127.0.0.1", f.port))
        payload = b"abc" * 100_000
        got = b""
        sender = threading.Thread(target=lambda: (c.sendall(payload), c.shutdown(socket.SHUT_WR)))
        sender.start()
        while True:
            d = c.recv(65536)
            if not d:
                break
            got += d
        sender.join()
        self.assertEqual(got, payload.upper())

    def test_concurrent_clients_are_independent(self):
        bp = self.backend(self.echo)
        f = self.fwd(lambda: socket.create_connection(("127.0.0.1", bp)))
        results = {}

        def client(i):
            c = socket.create_connection(("127.0.0.1", f.port))
            c.sendall(f"hello-{i}".encode())
            results[i] = c.recv(100)
            c.close()
        ts = [threading.Thread(target=client, args=(i,)) for i in range(12)]
        [t.start() for t in ts]
        [t.join() for t in ts]
        self.assertEqual(results, {i: f"HELLO-{i}".encode() for i in range(12)})

    def test_an_unreachable_backend_closes_the_client_instead_of_hanging(self):
        def boom():
            raise OSError("no route into the namespace")
        f = self.fwd(boom)
        c = socket.create_connection(("127.0.0.1", f.port))
        c.settimeout(5)
        self.assertEqual(c.recv(10), b"")

    def test_netns_connector_joins_the_namespace_in_a_throwaway_thread(self):
        bp = self.backend(self.echo)
        d = tmp(self)
        os.makedirs(os.path.join(d, "321", "ns"))
        open(os.path.join(d, "321", "ns", "net"), "w").close()
        seen = {}

        def fake_setns(fd, flag):
            seen["thread"] = threading.get_ident()
            seen["flag"] = flag
            seen["fd_open"] = os.fstat(fd) is not None
        s = px.netns_connector(321, bp, proc=d, setns=fake_setns)()
        self.assertNotEqual(seen["thread"], threading.get_ident())      # the caller's thread never changes namespace
        self.assertEqual(seen["flag"], getattr(os, "CLONE_NEWNET", 0x40000000))
        self.assertTrue(seen["fd_open"])
        s.sendall(b"ping")
        self.assertEqual(s.recv(10), b"PING")
        s.close()

    def test_netns_connector_errors_carry_context(self):
        d = tmp(self)
        os.makedirs(os.path.join(d, "9", "ns"))
        open(os.path.join(d, "9", "ns", "net"), "w").close()

        def bad(fd, flag):
            raise PermissionError("EPERM")
        with self.assertRaises(OSError) as cm:
            px.netns_connector(9, 5000, proc=d, setns=bad)()
        self.assertIn("pid 9", str(cm.exception))
        with self.assertRaises(OSError):
            px.netns_connector(77, 5000, proc=d, setns=bad)()      # no such pid


# -------------------------------------------------------------------------------------------- arm
class ArmEnv:
    def __init__(self, tc, **rootfs_kw):
        self.d = tmp(tc)
        self.lower = make_rootfs(os.path.join(self.d, "lower"), **rootfs_kw)
        insp = ri.inspect_rootfs(self.lower, pins.IMAGE_QCOW_SHA256)
        self.insp_path = os.path.join(self.d, "inspection.json")
        json.dump(insp, open(self.insp_path, "w"))
        self.state = os.path.join(self.d, "state")
        self.cfg_path = os.path.join(self.d, "config.json")
        self.cfg_dict = {"state_dir": self.state, "lower": self.lower, "inspection": self.insp_path,
                         "image_sha256": pins.IMAGE_QCOW_SHA256, "vcpus": 4, "memory_mib": 4096, "screen": "1920x1080x24",
                         "profile": "gnome_x11", "service_base_port": 26000, "guest_service_port": 5000}

    def write(self, **over):
        c = {**self.cfg_dict, **over}
        json.dump(c, open(self.cfg_path, "w"))
        return self.cfg_path

    def arm(self, world=None, **kw):
        world = world or World()
        ex = arm_exec(world)
        cfg = armmod.load_config(self.write())

        def http(url, timeout=5.0):
            if world.f["server_dead"] or not (world.trees and world.proxies):
                raise OSError("connection refused")
            return "Linux"
        kw.setdefault("cgroup_reader", fake_cgroup_reader(world))
        kw.setdefault("userns_host", FakeUserns())
        a = armmod.Arm(cfg, ex, FakeHost(world, "tree"), FakeHost(world, "proxy"), sleep=NO_SLEEP, http=http,
                       wait_s=kw.pop("wait_s", 0.05), controllers_path=kw.pop("controllers", os.path.join(self.d, "ctl")), **kw)
        return a, world, ex


class ArmConfigTests(unittest.TestCase):
    def test_a_valid_config_loads(self):
        e = ArmEnv(self)
        self.assertEqual(armmod.load_config(e.write())["vcpus"], 4)

    def test_refusals(self):
        e = ArmEnv(self)
        for over, text in (({"image_sha256": "0" * 64}, "exact same image"), ({"profile": "x"}, "profile"),
                           ({"screen": "wide"}, "screen"), ({"vcpus": True}, "vcpus"), ({"memory_mib": 0}, "memory_mib"),
                           ({"service_base_port": -1}, "service_base_port"), ({"lower": "/nonexistent"}, "not a directory"),
                           ({"inspection": "/nonexistent.json"}, "inspection")):
            with self.assertRaises(armmod.ArmError, msg=over) as cm:
                armmod.load_config(e.write(**over))
            self.assertIn(text, str(cm.exception))
        with self.assertRaises(armmod.ArmError):
            armmod.load_config(os.path.join(e.d, "missing.json"))
        c = dict(e.cfg_dict)
        del c["screen"]
        json.dump(c, open(e.cfg_path, "w"))
        with self.assertRaises(armmod.ArmError):
            armmod.load_config(e.cfg_path)

    def test_an_inspection_that_is_not_ok_or_of_another_root_is_refused(self):
        e = ArmEnv(self, user=False)
        with self.assertRaises(armmod.ArmError) as cm:
            armmod.load_config(e.write())
        self.assertIn("not ok_for_server", str(cm.exception))
        e2 = ArmEnv(self)
        other = make_rootfs(os.path.join(e2.d, "other"))
        json.dump(ri.inspect_rootfs(other, pins.IMAGE_QCOW_SHA256), open(e2.insp_path, "w"))
        with self.assertRaises(armmod.ArmError) as cm2:
            armmod.load_config(e2.write())
        self.assertIn("different root", str(cm2.exception))


class ArmVerbTests(unittest.TestCase):
    def test_describe_states_resources_pins_and_confounds(self):
        a, w, ex = ArmEnv(self).arm()
        d = a.describe()
        self.assertEqual(d["image_sha256"], pins.IMAGE_QCOW_SHA256)
        self.assertEqual(d["resources"]["cgroup_properties"][:2], ["CPUQuota=400%", "MemoryMax=4096M"])
        self.assertEqual(d["fidelity_differences"], FIDELITY_DIFFERENCES)
        self.assertEqual(d["user"], "user")
        self.assertFalse(d["server_matches_pinned_public_source"])
        self.assertEqual(json.dumps(d, sort_keys=True), json.dumps(a.describe(), sort_keys=True))   # stable (it is hashed)
        self.assertEqual(ex.calls, [])                                                         # describe runs nothing

    def test_provision_sweeps_first_plans_launches_proxies_and_waits_for_the_server(self):
        e = ArmEnv(self)
        a, w, ex = e.arm()
        rec = a.provision(0)
        kinds = [x[0] for x in w.events]
        self.assertEqual(kinds, ["launch", "proxy"])
        marker = L.marker_for(e.state, 0)
        launch = w.last_launch
        self.assertIn("CPUQuota=400%", launch)
        self.assertEqual(launch[launch.index("--marker") + 1], marker)
        plan = json.load(open(L.slot_paths(e.state, 0)["plan"]))
        self.assertEqual(plan["server"]["argv"], ["/usr/bin/python3", "/home/user/server/main.py"])
        self.assertEqual(plan["lower"], e.lower)
        self.assertEqual(rec["port"], 26000)
        self.assertEqual(rec["state"], "running")
        self.assertEqual(a.endpoint(0), {"BENCH_SERVICE_PORT": 26000, "BENCH_SLOT": 0, "OSWORLD_READY_DISPLAY_PROCS": "Xvfb"})

    def test_provision_refuses_unsupported_plan_before_launching(self):
        e = ArmEnv(self, xvfb=False)
        a, w, ex = e.arm()
        with self.assertRaises(armmod.ArmError) as cm:
            a.provision(0)
        self.assertIn("no headless display", str(cm.exception))
        self.assertEqual(w.events, [])
        self.assertEqual(ex.calls, [])

    def test_provision_failure_sweeps_the_half_started_slot(self):
        e = ArmEnv(self)
        a, w, ex = e.arm(World(server_dead=True))
        with self.assertRaises(armmod.ArmError) as cm:
            a.provision(0)
        self.assertIn("guest server not answering", str(cm.exception))
        self.assertEqual((w.trees, w.proxies), ({}, {}))           # nothing left running

    def test_provision_with_no_init_ever_found_fails_and_sweeps(self):
        e = ArmEnv(self)
        a, w, ex = e.arm(World(no_init=True))
        with self.assertRaises(armmod.ArmError) as cm:
            a.provision(0)
        self.assertIn("slot init not found", str(cm.exception))
        self.assertEqual(w.trees, {})

    def test_provision_refuses_leftovers_it_cannot_clear_and_nonempty_state(self):
        e = ArmEnv(self)
        w = World(unkillable={5001})
        marker = L.marker_for(e.state, 0)
        w.trees[5001] = marker
        a, _, _ = e.arm(w)
        with self.assertRaises(armmod.ArmError) as cm:
            a.provision(0)
        self.assertIn("could not be cleared", str(cm.exception))
        e2 = ArmEnv(self)
        p = L.slot_paths(e2.state, 0)
        os.makedirs(p["upper"])
        open(os.path.join(p["upper"], "stale"), "w").write("x")
        a2, w2, _ = e2.arm()
        with self.assertRaises(armmod.ArmError) as cm2:
            a2.provision(0)
        self.assertIn("not empty", str(cm2.exception))
        self.assertEqual(w2.events, [])

    def provisioned(self, **wflags):
        e = ArmEnv(self)
        a, w, ex = e.arm(World(**wflags))
        a.provision(0)
        p = L.slot_paths(e.state, 0)
        os.makedirs(os.path.join(p["upper"], "home/user/Desktop"), exist_ok=True)
        open(os.path.join(p["upper"], "home/user/Desktop/baseline.txt"), "w").write("clean")
        return e, a, w, ex, p

    def test_snapshot_dumps_without_leave_running_copies_the_disk_layer_then_restores(self):
        e, a, w, ex, p = self.provisioned()
        w.events.clear()
        a.snapshot(0)
        order = [x[0] for x in w.events]
        self.assertEqual(order[:3], ["dump", "terminate", "cp"] if "terminate" in order[:3] else ["dump", "cp", "restore"][:3])
        self.assertLess(order.index("dump"), order.index("cp"))
        self.assertLess(order.index("cp"), order.index("restore"))
        self.assertLess(order.index("restore"), order.index("proxy"))
        self.assertEqual(w.events[order.index("dump")], ("dump", False))                  # tree stopped for a consistent copy
        self.assertTrue(os.path.isfile(os.path.join(p["dir"], "upper.snap/home/user/Desktop/baseline.txt")))
        self.assertEqual(len(w.proxies), 1)
        self.assertEqual(len(w.trees), 1)

    def test_snapshot_failures_and_double_snapshot_are_refused(self):
        e, a, w, ex, p = self.provisioned(dump_fails=True)
        with self.assertRaises(armmod.ArmError):
            a.snapshot(0)
        self.assertFalse(os.path.exists(os.path.join(p["dir"], "upper.snap")))
        e2, a2, w2, ex2, p2 = self.provisioned()
        a2.snapshot(0)
        with self.assertRaises(armmod.ArmError):
            a2.snapshot(0)

    def test_restore_puts_the_saved_disk_layer_back_and_restarts_proxy_on_the_new_init(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        open(os.path.join(p["upper"], "home/user/Desktop/baseline.txt"), "w").write("DIRTY")
        open(os.path.join(p["upper"], "home/user/Desktop/new-folder"), "w").write("x")
        w.events.clear()
        a.restore(0)
        self.assertEqual(open(os.path.join(p["upper"], "home/user/Desktop/baseline.txt")).read(), "clean")
        self.assertFalse(os.path.exists(os.path.join(p["upper"], "home/user/Desktop/new-folder")))
        restore_pid = next(x[1] for x in w.events if x[0] == "restore")
        self.assertEqual(next(x[1] for x in w.events if x[0] == "proxy"), restore_pid)   # proxy follows the new init
        self.assertEqual((len(w.trees), len(w.proxies)), (1, 1))

    def test_reset_is_restore_and_requires_a_snapshot(self):
        self.assertIs(armmod.Arm.reset, armmod.Arm.restore)
        e, a, w, ex, p = self.provisioned()
        with self.assertRaises(armmod.ArmError) as cm:
            a.reset(0)
        self.assertIn("no snapshot", str(cm.exception))

    def test_restore_failure_is_reported(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        w.f["restore_fails"] = True
        with self.assertRaises(armmod.ArmError):
            a.restore(0)

    def test_cleanup_removes_only_heavy_dirs_under_the_state_dir_and_keeps_evidence(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        rep = a.cleanup()
        self.assertTrue(rep["ok"])
        self.assertEqual(set(rep["slots"]["slot0"]["removed"]), {"upper", "upper.snap", "work", "merged", "images"})
        for kept in ("service-plan.json", "slot.json"):
            self.assertTrue(os.path.exists(os.path.join(p["dir"], kept)), kept)
        self.assertEqual((w.trees, w.proxies), ({}, {}))
        self.assertTrue(os.path.isdir(e.lower))                                           # the image tree is never touched

    def test_cleanup_keeps_everything_when_a_process_cannot_be_stopped(self):
        e, a, w, ex, p = self.provisioned()
        pid = next(iter(w.trees))
        w.f["unkillable"].add(pid)
        rep = a.cleanup()
        self.assertFalse(rep["ok"])
        self.assertTrue(os.path.isdir(p["upper"]))
        self.assertIn("evidence", rep["slots"]["slot0"])

    def test_cleanup_never_deletes_a_mount_point(self):
        e, a, w, ex, p = self.provisioned()
        real = ex.fn

        def fn(argv):
            if argv[0] == "findmnt":
                return Res(0)
            return real(argv)
        ex.fn = fn
        rep = a.cleanup()
        self.assertFalse(rep["ok"])
        self.assertIn("mount point", rep["slots"]["slot0"]["why"])
        self.assertTrue(os.path.isdir(p["merged"]))

    # ---- restore stays inside a verified constrained cgroup -------------------------------------------
    def test_restore_runs_criu_inside_a_fresh_constrained_scope_and_reads_the_limits_back(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        w.events.clear()
        a.restore(0)
        scoped = [x for x in w.events if x[0] == "scoped_restore"]
        self.assertEqual(len(scoped), 1)
        argv = scoped[0][2]
        self.assertEqual(argv[0], "systemd-run")
        for prop in ("CPUQuota=400%", "MemoryMax=4096M", "MemorySwapMax=0"):
            self.assertIn(prop, argv)
        self.assertEqual(argv[argv.index("--") + 1:][:2], ["criu", "restore"])      # criu itself is inside the scope
        self.assertTrue(scoped[0][1].endswith("-r2.scope"))   # snapshot's own restore was r1
        rec = json.load(open(os.path.join(p["dir"], "slot.json")))
        self.assertEqual(rec["cgroup"]["cpu_max"], "400000 100000")
        self.assertEqual(rec["cgroup"]["memory_max"], str(4096 * 1048576))
        a.restore(0)
        self.assertTrue([x for x in w.events if x[0] == "scoped_restore"][-1][1].endswith("-r3.scope"))   # a new scope per restore

    def test_a_restore_that_lands_outside_the_scope_is_swept_not_declared_ready(self):
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        real = ex.fn

        def fn(argv):                       # a bare criu restore (what the old code ran): no scope unit
            if argv[0] == "systemd-run" and "criu" in argv:
                return real(argv[argv.index("--") + 1:])
            return real(argv)
        ex.fn = fn
        with self.assertRaises(armmod.ArmError) as cm:
            a.restore(0)
        self.assertIn("not under the preregistered", str(cm.exception))
        self.assertEqual((w.trees, w.proxies), ({}, {}))

    def test_cgroup_readback_that_differs_or_is_unreadable_fails_closed_for_launch_and_restore(self):
        for flag, text in (({"cgroup_limits": {"cpu_max": "max 100000"}}, "cpu_max"),
                           ({"cgroup_limits": {"memory_max": "max"}}, "memory_max"),
                           ({"cgroup_limits": {"swap_max": "max"}}, "swap_max"),
                           ({"stray_descendants": [9999]}, "outside"),
                           ({"cgroup_unreadable": True}, "cannot read back")):
            e = ArmEnv(self)
            world = World(**flag)
            a, w, ex = e.arm(world)
            with self.assertRaises(armmod.ArmError, msg=flag) as cm:
                a.provision(0)
            self.assertIn(text, str(cm.exception))
            self.assertEqual((w.trees, w.proxies), ({}, {}), flag)
            self.assertFalse([x for x in w.events if x[0] == "proxy"], flag)         # never reached the proxy

    def test_restore_with_unreadable_or_wrong_limits_fails_closed(self):
        for flag, text in (({"cgroup_limits": {"memory_max": "max"}}, "memory_max"), ({"cgroup_unreadable": True}, "cannot read back")):
            e, a, w, ex, p = self.provisioned()
            a.snapshot(0)
            w.f.update(flag)
            with self.assertRaises(armmod.ArmError) as cm:
                a.restore(0)
            self.assertIn(text, str(cm.exception))
            self.assertEqual((w.trees, w.proxies), ({}, {}))

    def test_cgroup_helpers_verify_names_limits_and_membership(self):
        ok = {"path": "/system.slice/drumsbench-m0.scope", "cpu_max": "400000 100000", "memory_max": str(4096 * 1048576),
              "swap_max": "0", "procs": [7], "descendants": [8], "descendant_cgroups": {"8": "/system.slice/drumsbench-m0.scope"}}
        self.assertEqual(L.verify_cgroup(ok, 7, "m0", 4, 4096), [])
        self.assertTrue(L.verify_cgroup({**ok, "path": "/user.slice/x.scope"}, 7, "m0", 4, 4096))
        self.assertTrue(L.verify_cgroup({**ok, "procs": []}, 7, "m0", 4, 4096))
        self.assertTrue(L.verify_cgroup(ok, 7, "m0", 2, 4096))
        r = L.restore_scoped_argv("m0", 3, 4, 4096, "/i", "/p", "/l")
        self.assertIn("drumsbench-m0-r3.scope", r)
        self.assertEqual(r[r.index("--") + 1:], L.restore_argv("/i", "/p", "/l"))

    def test_read_cgroup_reads_a_proc_and_cgroupfs_tree(self):
        d = tmp(self)
        proc, cg = os.path.join(d, "proc"), os.path.join(d, "cg")
        os.makedirs(os.path.join(proc, "7/task/7"))
        os.makedirs(os.path.join(cg, "system.slice/drumsbench-m0.scope"))
        open(os.path.join(proc, "7/cgroup"), "w").write("0::/system.slice/drumsbench-m0.scope\n")
        open(os.path.join(proc, "7/task/7/children"), "w").write("")
        open(os.path.join(proc, "7/stat"), "w").write("7 (x) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 100 0 0\n")
        sc = os.path.join(cg, "system.slice/drumsbench-m0.scope")
        for n, v in (("cgroup.procs", "7\n"), ("cpu.max", "400000 100000\n"), ("memory.max", str(4096 * 1048576) + "\n"),
                     ("memory.swap.max", "0\n")):
            open(os.path.join(sc, n), "w").write(v)
        info = L.read_cgroup(7, proc, cg)
        self.assertEqual(L.verify_cgroup(info, 7, "m0", 4, 4096), [])
        os.remove(os.path.join(sc, "memory.max"))
        with self.assertRaises(OSError):
            L.read_cgroup(7, proc, cg)

    def proc_tree(self, children="", start="100", stray=None):
        d = tmp(self)
        proc, cg = os.path.join(d, "proc"), os.path.join(d, "cg")
        sc = os.path.join(cg, "system.slice/drumsbench-m0.scope")
        os.makedirs(sc)
        for n, v in (("cgroup.procs", "7\n"), ("cpu.max", "400000 100000\n"), ("memory.max", str(4096 * 1048576) + "\n"),
                     ("memory.swap.max", "0\n")):
            open(os.path.join(sc, n), "w").write(v)

        def mk(pid, kids, cgpath="/system.slice/drumsbench-m0.scope", st="100"):
            os.makedirs(os.path.join(proc, f"{pid}/task/{pid}"), exist_ok=True)
            open(os.path.join(proc, f"{pid}/cgroup"), "w").write(f"0::{cgpath}\n")
            open(os.path.join(proc, f"{pid}/task/{pid}/children"), "w").write(kids)
            open(os.path.join(proc, f"{pid}/stat"), "w").write(f"{pid} (x) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 {st} 0 0\n")
        mk(7, children, st=start)
        return d, proc, cg, mk

    def test_a_child_that_exits_mid_read_is_retried_away_not_an_error(self):
        d, proc, cg, mk = self.proc_tree(children="99 ")       # 99 is listed but /proc/99 is already gone (the real failure)
        calls = []

        def settle(s):                                           # the kernel's children list catches up
            calls.append(s)
            open(os.path.join(proc, "7/task/7/children"), "w").write("")
        info = L.read_cgroup(7, proc, cg, sleep=settle)
        self.assertEqual(info["descendants"], [])
        self.assertEqual(info["snapshot_attempts"], 2)
        self.assertEqual(len(calls), 1)
        self.assertEqual(L.verify_cgroup(info, 7, "m0", 4, 4096), [])

    def test_a_live_stray_descendant_is_still_reported_not_ignored(self):
        d, proc, cg, mk = self.proc_tree(children="55 ")
        mk(55, "", cgpath="/user.slice/escaped.scope")          # alive, in ANOTHER cgroup
        info = L.read_cgroup(7, proc, cg, sleep=lambda s: None)
        self.assertEqual(info["descendants"], [55])
        problems = L.verify_cgroup(info, 7, "m0", 4, 4096)
        self.assertTrue(any("outside" in x for x in problems))

    def test_persistent_churn_or_an_init_that_changes_identity_fails_closed(self):
        d, proc, cg, mk = self.proc_tree(children="99 ")
        slept = []
        with self.assertRaises(OSError) as cm:
            L.read_cgroup(7, proc, cg, attempts=3, sleep=slept.append)
        self.assertIn("no clean snapshot", str(cm.exception))
        self.assertEqual(len(slept), 3)                          # bounded
        d, proc, cg, mk = self.proc_tree(children="99 ")

        def swap_init(s):                                        # pid 7 is replaced by another process mid-read
            open(os.path.join(proc, "7/task/7/children"), "w").write("")
            open(os.path.join(proc, "7/stat"), "w").write("7 (x) S 1 1 1 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 555 0 0\n")
        with self.assertRaises(OSError) as cm2:
            L.read_cgroup(7, proc, cg, sleep=swap_init)
        self.assertIn("changed identity", str(cm2.exception))
        with self.assertRaises(OSError):                         # init itself gone
            L.read_cgroup(8, proc, cg)

    def test_a_kernel_without_the_children_file_still_fails_closed(self):
        d, proc, cg, mk = self.proc_tree()
        os.remove(os.path.join(proc, "7/task/7/children"))
        with self.assertRaises(OSError):
            L.read_cgroup(7, proc, cg, sleep=lambda s: None)

    def test_a_thread_that_exits_mid_read_of_a_live_descendant_is_retried_not_an_error(self):
        # lab-scale-bench 2026-10-29: "cannot read back the cgroup of init pid 657972: [Errno 2] No such file or directory:
        # '/proc/658018/task/658447/children'" -- descendant 658018 was alive; its THREAD 658447 exited between listdir(task) and the read.
        for owner in ("55", "7"):                                # a thread of a live descendant, and a thread of the init itself
            d, proc, cg, mk = self.proc_tree(children="55 ")
            mk(55, "")
            real, ghost = os.listdir, {"left": 1}

            def listdir(path, owner=owner):                      # the first snapshot lists thread 999, which has already exited
                out = real(path)
                if path == os.path.join(proc, owner, "task") and ghost["left"]:
                    ghost["left"] -= 1
                    out = out + ["999"]
                return out
            with mock.patch.object(L.os, "listdir", listdir):
                info = L.read_cgroup(7, proc, cg, sleep=lambda s: None)
            self.assertEqual(info["descendants"], [55], owner)
            self.assertEqual(info["snapshot_attempts"], 2, owner)  # the racy snapshot is retried, not trusted and not fatal
            self.assertEqual(L.verify_cgroup(info, 7, "m0", 4, 4096), [], owner)

    def test_a_live_thread_without_a_children_file_still_fails_closed(self):
        d, proc, cg, mk = self.proc_tree(children="55 ")
        mk(55, "")
        os.makedirs(os.path.join(proc, "55/task/56/x"))          # thread directory present, children file missing: not a race
        with self.assertRaises(OSError):
            L.read_cgroup(7, proc, cg, sleep=lambda s: None)

    def test_a_failed_criu_dump_or_restore_reports_the_error_lines_of_its_log(self):
        # lab-scale-bench layer d: "criu dump (...) failed rc=1 error=None stderr=''" -- criu's diagnosis was only in dump.log
        e, a, w, ex, p = self.provisioned()
        w.f["dump_fails_quiet"] = True
        with self.assertRaises(armmod.ArmError) as cm:
            a.snapshot(0)
        self.assertIn("Error (criu/cr-dump.c:1): fake dump error", str(cm.exception))
        self.assertIn("dump.log", str(cm.exception))
        e, a, w, ex, p = self.provisioned()
        a.snapshot(0)
        w.f["restore_fails_quiet"] = True
        with self.assertRaises(armmod.ArmError) as cm:
            a.restore(0)
        self.assertIn("Too many open files", str(cm.exception))

    def test_criu_log_evidence_only_for_criu_commands_with_a_log(self):
        d = tmp(self)
        log = os.path.join(d, "x.log")
        self.assertEqual(armmod.criu_log_evidence(("cp", "-o", log)), "")
        self.assertIn("unreadable", armmod.criu_log_evidence(("criu", "dump", "-o", log)))
        open(log, "w").write("a\nb\nc\nd\n")
        self.assertIn("last lines: b | c | d", armmod.criu_log_evidence(("/opt/c/criu", "dump", "-o", log)))

    def test_a_restore_that_hit_the_inotify_limit_names_the_host_setting(self):
        d = tmp(self)
        log = os.path.join(d, "r.log")
        open(log, "w").write("(00.31) Error (criu/fsnotify.c:1486): fsnotify: Can't create inotify fd: Too many open files\n")
        lim = os.path.join(d, "max_user_instances")
        open(lim, "w").write("8192\n")
        with mock.patch.object(armmod, "INOTIFY_SYSCTL", lim):
            ev = armmod.criu_log_evidence(("criu", "restore", "-o", log))
        self.assertIn("Can't create inotify fd", ev)
        self.assertIn("fs.inotify.max_user_instances=8192", ev)
        open(log, "w").write("(00.31) Error (criu/files.c:1): something else: Too many open files\n")
        self.assertNotIn("max_user_instances", armmod.criu_log_evidence(("criu", "restore", "-o", log)))

    def test_service_logs_survive_a_failed_provision_and_cleanup(self):
        e = ArmEnv(self)
        w = World(cgroup_limits={"memory_max": "max"}, service_log="desktop.log: Gio.IOErrorEnum")
        a, w, ex = e.arm(w)
        with self.assertRaises(armmod.ArmError):
            a.provision(0)
        p = L.slot_paths(e.state, 0)
        kept = [d for d in os.listdir(p["dir"]) if d.startswith("service-logs-provision-")]
        self.assertEqual(len(kept), 1)
        self.assertIn("Gio.IOErrorEnum", open(os.path.join(p["dir"], kept[0], "desktop.log")).read())
        self.assertTrue(a.cleanup()["ok"])
        self.assertFalse(os.path.exists(p["upper"]))
        self.assertTrue(any(d.startswith("service-logs-at-cleanup") for d in os.listdir(p["dir"])))   # evidence outlives the upper

    def test_the_inspector_cli_records_the_attached_image_hash_and_the_arm_requires_it(self):
        d = tmp(self)
        root = make_rootfs(os.path.join(d, "root"))
        state = os.path.join(d, "mount.json")
        json.dump({"image_sha256": pins.IMAGE_QCOW_SHA256}, open(state, "w"))
        out = os.path.join(d, "insp.json")
        r = subprocess.run([sys.executable, "-m", "sharedos_rootfs.rootfs_inspect", root, "--out", out, "--mount-state", state],
                           cwd=PKG_ROOT, capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.load(open(out))["image_sha256"], pins.IMAGE_QCOW_SHA256)
        self.assertEqual(json.loads(r.stdout)["server_sha256"], hashlib.sha256(SERVER_SRC).hexdigest())
        bad = os.path.join(d, "bad.json")
        json.dump({"image_sha256": "nothex"}, open(bad, "w"))
        r2 = subprocess.run([sys.executable, "-m", "sharedos_rootfs.rootfs_inspect", root, "--out", os.path.join(d, "i2.json"),
                             "--mount-state", bad], cwd=PKG_ROOT, capture_output=True, text=True)
        self.assertNotEqual(r2.returncode, 0)

    def test_config_requires_the_inspection_to_carry_the_pinned_image_hash(self):
        for sha in (None, "0" * 64):
            e = ArmEnv(self)
            json.dump(ri.inspect_rootfs(e.lower, sha), open(e.insp_path, "w"))
            with self.assertRaises(armmod.ArmError) as cm:
                armmod.load_config(e.write())
            self.assertIn("pinned image sha256", str(cm.exception))

    def test_environment_fails_closed(self):
        e = ArmEnv(self)
        ctl = os.path.join(e.d, "ctl")
        a, w, ex = e.arm(controllers=ctl)
        with mock.patch.object(armmod, "_has_setns", lambda: True):   # independent of the running Python version
            with self.assertRaises(armmod.ArmError) as cm:
                a.environment()
            self.assertIn("cgroup v2", str(cm.exception))
            open(ctl, "w").write("cpuset cpu io memory pids\n")
            self.assertIn("cgroup controllers", a.environment())
            open(ctl, "w").write("cpuset io\n")
            with self.assertRaises(armmod.ArmError):
                a.environment()
            a2, w2, ex2 = e.arm(controllers=ctl)
            a2.th.tools.discard("criu")
            open(ctl, "w").write("cpu memory\n")
            with self.assertRaises(armmod.ArmError) as cm2:
                a2.environment()
            self.assertIn("criu", str(cm2.exception))

    def test_python_without_setns_is_refused_by_environment_and_by_the_proxy(self):
        e = ArmEnv(self)
        ctl = os.path.join(e.d, "ctl")
        open(ctl, "w").write("cpu memory\n")
        a, w, ex = e.arm(controllers=ctl)
        with mock.patch.object(armmod, "_has_setns", lambda: False), mock.patch.object(px, "_has_setns", lambda: False):
            with self.assertRaises(armmod.ArmError) as cm:
                a.environment()
            self.assertIn("os.setns", str(cm.exception))
            self.assertEqual(px.main(["--marker", "m", "--init-pid", "1", "--listen-port", "1", "--target-port", "1"]), 2)

    def test_endpoint_requires_a_provisioned_slot(self):
        a, w, ex = ArmEnv(self).arm()
        with self.assertRaises(armmod.ArmError):
            a.endpoint(0)

    def test_main_dispatch_and_errors(self):
        e = ArmEnv(self)
        w = World()
        env = {"SHAREDOS_ROOTFS_CONFIG": e.write()}
        with mock.patch.dict(os.environ, env):
            run = lambda *argv: armmod.main(list(argv), executor=arm_exec(w), tree_host=FakeHost(w, "tree"),  # noqa: E731
                                            proxy_host=FakeHost(w, "proxy"))
            self.assertEqual(run("describe"), 0)
            self.assertEqual(run("bogus", "--slot", "0"), 1)
            self.assertEqual(run("provision"), 1)                      # --slot required
            self.assertEqual(run("endpoint", "--slot", "0"), 1)        # never provisioned
            self.assertEqual(run("cleanup"), 0)
        with mock.patch.dict(os.environ, {"SHAREDOS_ROOTFS_CONFIG": "/nonexistent"}):
            self.assertEqual(armmod.main(["describe"]), 1)


class ArmCliAndProtocolTests(unittest.TestCase):
    """The shim must speak the drumsbench external-arm protocol (describe is hashed, environment fails closed)."""

    def run_cli(self, e, *args):
        env = {**os.environ, "SHAREDOS_ROOTFS_CONFIG": e.write()}
        return subprocess.run([sys.executable, "-m", "sharedos_rootfs.arm", *args], cwd=PKG_ROOT, env=env,
                              capture_output=True, text=True)

    def test_describe_is_json_and_hashable_by_the_external_arm_protocol(self):
        from drumsbench.external import ExternalArm, describe_hash
        e = ArmEnv(self)
        r = self.run_cli(e, "describe")
        self.assertEqual(r.returncode, 0, r.stderr)
        d = json.loads(r.stdout)
        self.assertEqual(d["arm"], "sharedos-rootfs")
        arm = ExternalArm("drums", [sys.executable, "-m", "sharedos_rootfs.arm"], e.d, describe_hash(r.stdout))
        self.assertEqual([c.argv[-1] for c in arm.capture_environment()], ["describe", "environment"])

    def test_environment_fails_closed_without_the_tools(self):
        e = ArmEnv(self)
        r = self.run_cli(e, "environment")
        if shutil.which("criu") is None:
            self.assertEqual(r.returncode, 1)
            self.assertIn("missing tools", r.stderr)

    def test_unprovisioned_slot_verbs_fail_with_a_message(self):
        e = ArmEnv(self)
        for verb in ("endpoint", "snapshot", "restore"):
            r = self.run_cli(e, verb, "--slot", "0")
            self.assertEqual(r.returncode, 1, verb)
            self.assertIn("never provisioned", r.stderr)


# ---------------------------------------------------------------------------------------- display staging
DPKG_STATUS = """Package: libc6
Status: install ok installed
Version: 2.35-0ubuntu3.4

Package: xserver-xorg-core
Status: install ok installed
Provides: xorg-video-abi-25, xserver-xorg-core-abi
Version: 2:21.1.4-2ubuntu1.7~22.04.8

"""


def stage_world(tc, status=DPKG_STATUS, driver=b"FAKE-DRIVER", extra_members=(), mode=0o644, deb_bytes=b"fake-deb"):
    """A fake lower root with a dpkg status, a fake .deb file and a fake `dpkg-deb` that returns synthetic data. Labelled
    FAKE: it proves the verification logic, not that the real deb or driver works."""
    import io
    import tarfile
    d = tmp(tc)
    lower = os.path.join(d, "lower")
    os.makedirs(os.path.join(lower, "var/lib/dpkg"))
    if status is not None:
        open(os.path.join(lower, "var/lib/dpkg/status"), "w").write(status)
    deb = os.path.join(d, "x.deb")
    open(deb, "wb").write(deb_bytes)
    pin = {**ds.PIN, "deb_sha256": hashlib.sha256(deb_bytes).hexdigest(), "deb_size": len(deb_bytes),
           "driver_sha256": hashlib.sha256(driver).hexdigest()}
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tf:
        def add(name, data=b"", typ=tarfile.REGTYPE, m=0o644):
            ti = tarfile.TarInfo(name)
            ti.type, ti.mode = typ, m
            ti.size = len(data) if typ == tarfile.REGTYPE else 0
            tf.addfile(ti, io.BytesIO(data) if typ == tarfile.REGTYPE else None)
        for dd in ("./", "./usr/", "./usr/lib/", "./usr/lib/xorg/", "./usr/lib/xorg/modules/", "./usr/lib/xorg/modules/drivers/"):
            add(dd, typ=tarfile.DIRTYPE, m=0o755)
        add("./usr/lib/xorg/modules/drivers/dummy_drv.so", driver, m=mode)
        for dd in ("./usr/share/", "./usr/share/bug/", "./usr/share/bug/xserver-xorg-video-dummy/", "./usr/share/doc/",
                   "./usr/share/doc/xserver-xorg-video-dummy/"):
            add(dd, typ=tarfile.DIRTYPE, m=0o755)           # the same entries the real deb lists (found on a real run)
        ti = tarfile.TarInfo("./usr/share/bug/xserver-xorg-video-dummy/script")
        ti.type, ti.linkname = tarfile.SYMTYPE, "../xserver-xorg-core/script"
        tf.addfile(ti)
        add("./usr/share/doc/xserver-xorg-video-dummy/copyright", b"c")
        for n in extra_members:
            add(n, b"x")
    tar_bytes = buf.getvalue()
    control = {"Package": pin["package"], "Version": pin["version"], "Architecture": pin["architecture"], "Depends": pin["depends"]}
    calls = []

    def run(argv):
        calls.append(argv)
        if argv[:2] == ["dpkg-deb", "-f"]:
            return 0, (control[argv[3]] + "\n").encode(), b""
        if argv[:2] == ["dpkg-deb", "--fsys-tarfile"]:
            return 0, tar_bytes, b""
        return 1, b"", b"unexpected"
    upper, slot = os.path.join(d, "upper"), os.path.join(d, "slot")
    os.makedirs(upper)
    os.makedirs(slot)
    return {"d": d, "lower": lower, "deb": deb, "pin": pin, "run": run, "control": control, "upper": upper, "slot": slot,
            "calls": calls}


class DisplayStageTests(unittest.TestCase):
    def go(self, w):
        return ds.stage(w["lower"], w["deb"], w["upper"], w["slot"], pin=w["pin"], run=w["run"])

    def test_the_pin_is_the_verified_public_jammy_package(self):
        self.assertEqual(ds.PIN["package"], "xserver-xorg-video-dummy")
        self.assertEqual(len(ds.PIN["deb_sha256"]), 64)
        self.assertEqual(ds.PIN["archive_provenance"]["suite"], "jammy")
        self.assertTrue(ds.PIN["deb_url"].startswith("http://archive.ubuntu.com/ubuntu/pool/main/"))

    def test_version_ordering_follows_debian_policy(self):
        v = ds.version_compare
        self.assertEqual(v("2:21.1.4-2ubuntu1.7~22.04.8", "2:21.1.1"), 1)
        self.assertEqual(v("1:0.3.8-2build1", "1:0.3.8-2build1"), 0)
        self.assertEqual(v("1.0~rc1", "1.0"), -1)
        self.assertEqual(v("1:1.0", "2.0"), 1)                 # epoch wins
        self.assertEqual(v("2.35-0ubuntu3.4", "2.4"), 1)       # numeric, not lexical
        self.assertEqual(v("1.0-1", "1.0-2"), -1)

    def test_dependency_checks_use_the_images_own_dpkg_database(self):
        st = ds.parse_dpkg_status(DPKG_STATUS)
        self.assertEqual(ds.check_depends(ds.PIN["depends"], st), [])
        self.assertTrue(ds.check_depends("libc6 (>= 9.0)", st))                            # too old
        self.assertTrue(ds.check_depends("xorg-video-abi-26", st))                         # ABI not provided
        self.assertTrue(ds.check_depends("not-installed", st))
        self.assertEqual(ds.check_depends("not-installed | libc6 (>= 2.4)", st), [])       # alternatives
        gone = ds.parse_dpkg_status(DPKG_STATUS.replace("install ok installed", "deinstall ok config-files"))
        self.assertTrue(ds.check_depends("libc6", gone))                                    # removed packages do not count
        self.assertTrue(ds.check_depends("libc6 (~~ 1)", st))                              # unparseable fails closed

    def test_stage_adds_one_file_to_the_upper_layer_and_never_touches_the_image(self):
        w = stage_world(self)
        before = sorted(os.walk(w["lower"]))
        rec = self.go(w)
        self.assertEqual(sorted(os.walk(w["lower"])), before)
        dest = os.path.join(w["upper"], ds.PIN["driver_path"])
        self.assertEqual(open(dest, "rb").read(), b"FAKE-DRIVER")
        self.assertEqual(oct(os.stat(dest).st_mode & 0o7777), "0o644")
        added = [os.path.relpath(os.path.join(r, f), w["upper"]) for r, _, fs in os.walk(w["upper"]) for f in fs]
        self.assertEqual(added, [ds.PIN["driver_path"]])           # docs were NOT staged
        self.assertFalse(rec["image_modified"])
        self.assertFalse(rec["host_binaries_copied"])
        self.assertEqual(rec["image_xserver_xorg_core_version"], "2:21.1.4-2ubuntu1.7~22.04.8")
        self.assertEqual(json.load(open(os.path.join(w["slot"], ds.RECORD_NAME)))["deb_sha256"], w["pin"]["deb_sha256"])

    def refuses(self, w, text):
        with self.assertRaises(ds.StageError) as cm:
            self.go(w)
        self.assertIn(text, str(cm.exception))
        self.assertEqual(os.listdir(w["upper"]), [])                # nothing staged on any refusal
        self.assertFalse(os.path.exists(os.path.join(w["slot"], ds.RECORD_NAME)))

    def test_every_unproven_condition_fails_closed_and_stages_nothing(self):
        w = stage_world(self)
        open(w["deb"], "ab").write(b"x")
        self.refuses(w, "size")
        w = stage_world(self)
        w["pin"]["deb_sha256"] = "0" * 64
        self.refuses(w, "sha256")
        w = stage_world(self)
        w["control"]["Version"] = "9"
        self.refuses(w, "control Version")
        self.refuses(stage_world(self, status=None), "dpkg status")
        self.refuses(stage_world(self, status="Package: libc6\nStatus: install ok installed\nVersion: 2.35\n"),
                     "xserver-xorg-core is not installed")
        self.refuses(stage_world(self, status=DPKG_STATUS.replace("xorg-video-abi-25", "xorg-video-abi-24")), "xorg-video-abi-25")
        self.refuses(stage_world(self, status=DPKG_STATUS.replace("2:21.1.4", "2:20.0.0")), "xserver-xorg-core")
        self.refuses(stage_world(self, extra_members=("./etc/cron.d/evil",)), "unexpected content")
        self.refuses(stage_world(self, extra_members=("./usr/bin/xterm-evil",)), "unexpected content")
        self.refuses(stage_world(self, mode=0o4755), "setuid")
        w = stage_world(self)
        w["pin"]["driver_sha256"] = "1" * 64
        self.refuses(w, "driver")

    def test_a_driver_the_image_already_has_is_not_overwritten_or_duplicated(self):
        w = stage_world(self)
        p = os.path.join(w["lower"], ds.PIN["driver_path"])
        os.makedirs(os.path.dirname(p))
        open(p, "w").write("image's own")
        self.refuses(w, "already contains")

    def test_dpkg_deb_failure_is_a_refusal(self):
        w = stage_world(self)
        w["run"] = lambda argv: (2, b"", b"boom")
        self.refuses(w, "dpkg-deb")

    def test_the_effective_inspection_shows_the_driver_and_keeps_the_original_untouched(self):
        insp = ri.inspect_rootfs(make_rootfs(tmp(self), xvfb=False, xorg=True, dummy=False), pins.IMAGE_QCOW_SHA256)
        orig = json.loads(json.dumps(insp))
        self.assertEqual(insp["display"]["decision"], "needs_added_display_package")
        rec = {"staged": True, "package": "p"}
        eff = ds.inspect_staged(insp, rec)
        self.assertEqual(eff["display"]["decision"], "xorg_dummy_staged")
        self.assertTrue(eff["display"]["xorg_dummy_driver"])
        self.assertEqual(eff["image_display"]["decision"], "needs_added_display_package")
        self.assertFalse(eff["image_display"]["xorg_dummy_driver"])      # what the image itself had
        self.assertEqual(insp, orig)                                       # the original inspection object is unchanged
        self.assertIs(ds.inspect_staged(insp, None), insp)
        with self.assertRaises(ds.StageError):
            ds.inspect_staged(ri.inspect_rootfs(make_rootfs(tmp(self)), pins.IMAGE_QCOW_SHA256), rec)   # image has Xvfb already
        noxorg = ri.inspect_rootfs(make_rootfs(tmp(self), xvfb=False), pins.IMAGE_QCOW_SHA256)
        with self.assertRaises(ds.StageError):
            ds.inspect_staged(noxorg, rec)

    def test_the_tree_runs_the_real_xorg_with_the_staged_driver_and_a_private_config(self):
        w = None
        e = ArmEnv(self, xvfb=False, xorg=True, dummy=False)
        eff = ds.inspect_staged(json.load(open(e.insp_path)), {"staged": True})
        eff["binaries"]["Xorg_real"] = "/usr/lib/xorg/Xorg"
        plan = armmod.build_plan(armmod.load_config(e.write()), 0, eff)
        disp = next(x for x in tr.services_plan(plan) if x["name"] == "display")
        self.assertEqual(disp["argv"][0], "/usr/lib/xorg/Xorg")
        self.assertIn("/tmp/xorg-dummy.conf", disp["argv"])
        self.assertIn("-nolisten", disp["argv"])


class ArmDisplayStageTests(unittest.TestCase):
    def env(self, with_deb=True, **kw):
        e = ArmEnv(self, xvfb=False, xorg=True, dummy=False, **kw)
        st = stage_world(self)
        if with_deb:
            e.cfg_dict["display_stage_deb"] = st["deb"]
        # the arm's lower dir must carry the dpkg status the stage reads
        os.makedirs(os.path.join(e.lower, "var/lib/dpkg"), exist_ok=True)
        open(os.path.join(e.lower, "var/lib/dpkg/status"), "w").write(DPKG_STATUS)
        return e, st

    def test_provision_stages_the_driver_into_the_slot_upper_and_records_it(self):
        e, st = self.env()
        a, w, ex = e.arm()
        with mock.patch.dict(ds.PIN, st["pin"]), mock.patch.object(ds, "_run", st["run"]):
            rec = a.provision(0)
        p = L.slot_paths(e.state, 0)
        self.assertTrue(os.path.isfile(os.path.join(p["upper"], ds.PIN["driver_path"])))
        self.assertFalse(os.path.exists(os.path.join(e.lower, ds.PIN["driver_path"])))        # the image is untouched
        plan = json.load(open(p["plan"]))
        self.assertEqual(plan["inspection"]["display"]["decision"], "xorg_dummy_staged")
        self.assertEqual(plan["inspection"]["display"]["added_by_stage"]["package"], ds.PIN["package"])
        slot = json.load(open(os.path.join(p["dir"], "slot.json")))
        self.assertEqual(slot["display_stage"]["files_added_to_upper"][0]["path"], "/" + ds.PIN["driver_path"])
        self.assertEqual(a.endpoint(0)["OSWORLD_READY_DISPLAY_PROCS"], "Xorg")
        self.assertEqual(a.describe()["display"]["decision"], "needs_added_display_package")     # the image's own, unedited
        self.assertTrue(a.describe()["display_stage"]["configured"])

    def test_the_staged_driver_survives_snapshot_and_restore(self):
        e, st = self.env()
        a, w, ex = e.arm()
        with mock.patch.dict(ds.PIN, st["pin"]), mock.patch.object(ds, "_run", st["run"]):
            a.provision(0)
        a.snapshot(0)
        p = L.slot_paths(e.state, 0)
        os.remove(os.path.join(p["upper"], ds.PIN["driver_path"]))        # a dirty attempt that destroyed it
        a.restore(0)
        self.assertTrue(os.path.isfile(os.path.join(p["upper"], ds.PIN["driver_path"])))

    def test_no_deb_configured_refuses_without_adding_anything(self):
        e, st = self.env(with_deb=False)
        a, w, ex = e.arm()
        with self.assertRaises(armmod.ArmError) as cm:
            a.provision(0)
        self.assertIn("nothing is added silently", str(cm.exception))
        self.assertEqual(w.events, [])

    def test_a_failed_stage_is_a_refusal_before_any_launch(self):
        e, st = self.env()
        a, w, ex = e.arm()
        st["pin"]["deb_sha256"] = "0" * 64
        with mock.patch.dict(ds.PIN, st["pin"]), mock.patch.object(ds, "_run", st["run"]):
            with self.assertRaises(armmod.ArmError) as cm:
                a.provision(0)
        self.assertIn("fail closed", str(cm.exception))
        self.assertFalse([x for x in w.events if x[0] == "launch"])

    def test_a_deb_for_an_image_that_has_its_own_display_is_refused(self):
        e = ArmEnv(self)
        e.cfg_dict["display_stage_deb"] = "/x.deb"
        a, w, ex = e.arm()
        with self.assertRaises(armmod.ArmError) as cm:
            a.provision(0)
        self.assertIn("own headless display", str(cm.exception))

    def test_config_rejects_a_non_string_deb_path(self):
        e = ArmEnv(self)
        with self.assertRaises(armmod.ArmError):
            armmod.load_config(e.write(display_stage_deb=5))

    def test_sameness_never_claims_identical_graphics_and_carries_the_stage_record(self):
        insp = ri.inspect_rootfs(make_rootfs(tmp(self)), pins.IMAGE_QCOW_SHA256)
        wl = mock.Mock(workload_hash="w" * 64)
        a = sm.facts_baseline(wl, {"vcpus": 4, "memory_mib": 4096}, insp, pins.IMAGE_QCOW_SHA256)
        b = sm.facts_rootfs(wl, {"vcpus": 4, "memory_mib": 4096}, insp, display_stage={"staged": True, "package": "p"})
        r = sm.compare(a, b)
        self.assertFalse(r["graphics"]["identical_graphics_claimed"])
        self.assertEqual(r["graphics"]["display_stage_record"]["package"], "p")
        self.assertIn("virtual GPU", r["graphics"]["baseline"])
        self.assertTrue(any("NOT identical" in x for x in r["declared_differences"]))


# --------------------------------------------------------------------------------------- sameness
class SamenessTests(unittest.TestCase):
    def setUp(self):
        self.insp = ri.inspect_rootfs(make_rootfs(tmp(self)))
        self.wl = mock.Mock(workload_hash="w" * 64)
        self.sha = pins.IMAGE_QCOW_SHA256

    def facts(self):
        return (sm.facts_baseline(self.wl, {"vcpus": 4, "memory_mib": 4096}, self.insp, self.sha),
                sm.facts_rootfs(self.wl, {"vcpus": 4, "memory_mib": 4096}, self.insp, self.sha))

    def test_identical_arms_have_no_blockers_but_still_carry_the_declared_differences(self):
        a, b = self.facts()
        r = sm.compare(a, b)
        self.assertTrue(r["identical"])
        self.assertEqual(set(r["matches"]), set(sm.IDENTITY_KEYS))
        self.assertEqual(r["declared_differences"], FIDELITY_DIFFERENCES)
        self.assertEqual(a["task_sha256"], pins.TASK_SHA256)

    def test_each_identity_fact_that_differs_blocks(self):
        for key, bad in (("image_sha256", "0" * 64), ("guest_vcpus", 2), ("guest_memory_mib", 2048), ("workload_hash", "x" * 64),
                         ("guest_server_sha256", "f" * 64), ("task_sha256", "e" * 64), ("evaluator_command_sha256", "d" * 64)):
            a, b = self.facts()
            b[key] = bad
            r = sm.compare(a, b)
            self.assertFalse(r["identical"], key)
            self.assertEqual([x["fact"] for x in r["blocking"]], [key])
            self.assertEqual(r["blocking"][0]["why"], "differs")

    def test_unknown_facts_block_instead_of_being_assumed_equal(self):
        a, b = self.facts()
        a["guest_server_sha256"] = None
        b["workload_hash"] = None
        r = sm.compare(a, b)
        self.assertEqual({x["fact"] for x in r["blocking"]}, {"guest_server_sha256", "workload_hash"})
        self.assertTrue(all("unknown" in x["why"] for x in r["blocking"]))

    def test_server_sameness_compares_the_actual_files_and_never_claims_pinned_equality_unless_true(self):
        a, b = self.facts()
        r = sm.compare(a, b)
        ev = r["evidence"]
        self.assertTrue(ev["pinned_source_equal"] is False)          # make_rootfs's server is not the pinned public file
        self.assertIn("NOT claimed", ev["statement"])
        self.assertEqual(set(ev["guest_server_sha256"].values()), {hashlib.sha256(SERVER_SRC).hexdigest()})
        self.assertEqual(ev["pinned_public_server_sha256"], PINNED_SERVER_MAIN_SHA256)
        self.assertTrue(r["identical"])                               # identical actual files still match each other
        # actual files that differ block, even though neither claims the pin
        b["guest_server_sha256"] = "9" * 64
        self.assertEqual([x["fact"] for x in sm.compare(a, b)["blocking"]], ["guest_server_sha256"])
        # both arms genuinely holding the pinned file is the only way the statement claims it
        a["guest_server_matches_pinned_public_source"] = b["guest_server_matches_pinned_public_source"] = True
        self.assertTrue(sm.compare(a, b)["evidence"]["pinned_source_equal"])
        self.assertNotIn("NOT claimed", sm.compare(a, b)["evidence"]["statement"])

    def test_rootfs_image_hash_comes_from_the_inspection_and_unknown_blocks(self):
        wl = self.wl
        with_hash = ri.inspect_rootfs(make_rootfs(tmp(self)), pins.IMAGE_QCOW_SHA256)
        self.assertEqual(sm.facts_rootfs(wl, {"vcpus": 4, "memory_mib": 4096}, with_hash)["image_sha256"], pins.IMAGE_QCOW_SHA256)
        none = sm.facts_rootfs(wl, {"vcpus": 4, "memory_mib": 4096}, self.insp)
        base = sm.facts_baseline(wl, {"vcpus": 4, "memory_mib": 4096}, self.insp, self.sha)
        self.assertEqual([x["fact"] for x in sm.compare(base, none)["blocking"]], ["image_sha256"])

    def test_evaluator_fact_is_stable_and_follows_the_pinned_task(self):
        self.assertEqual(sm.evaluator_sha(), sm.evaluator_sha())
        self.assertEqual(len(sm.evaluator_sha()), 64)


class FakeSeccomp:
    """Stand-in for python3-seccomp (libseccomp): records the default action and every (action, syscall) rule, and
    whether the filter was loaded. ALLOW/ERRNO mirror the real module's API shape."""
    ALLOW = ("ALLOW",)

    @staticmethod
    def ERRNO(n):
        return ("ERRNO", n)

    class SyscallFilter:
        def __init__(self, defaction):
            self.defaction, self.rules, self.loaded = defaction, [], False

        def add_rule(self, action, name):
            self.rules.append((action, name))

        def load(self):
            self.loaded = True


class SlotHardeningTests(unittest.TestCase):
    """The escape-prevention controls as data: these fail against the pre-fix build (identity uid map, full caps,
    no seccomp, no metadata block) and pass once harden.py is in place. Runtime proof (uid_map/CapEff/Seccomp=2/
    EPERM/metadata-unreachable in a real slot) is the Linux-host re-verification; these lock the configuration."""

    def test_user_namespace_map_is_unprivileged_and_disjoint_per_slot(self):
        self.assertEqual(H.subid_base(0), H.SUBID_BASE)
        self.assertGreaterEqual(H.SUBID_BASE, 100000)                 # above every system/human uid
        self.assertNotEqual(H.subid_base(0), H.subid_base(1))
        self.assertEqual(H.subid_base(1) - H.subid_base(0), H.SUBID_COUNT)
        self.assertEqual(H.uid_map_arg(0).split(",")[1], "0")         # inner root ...
        self.assertNotEqual(H.uid_map_arg(0).split(",")[0], "0")      # ... maps to a non-zero host uid
        self.assertEqual(H.userns_unshare_flags(0)[0], "--user")
        # /etc/subuid|subgid delegation newuidmap requires: covers every slot's host range
        su, sg = H.subid_delegation_lines("root", max_slots=100)
        self.assertEqual(su, sg)
        _, base, count = su.split(":")
        self.assertEqual(int(base), H.SUBID_BASE)
        self.assertGreaterEqual(int(count), 100 * H.SUBID_COUNT)

    def test_dangerous_syscalls_are_denied_and_chrome_sandbox_calls_are_not(self):
        for s in ("init_module", "finit_module", "delete_module", "kexec_load", "kexec_file_load", "bpf",
                  "perf_event_open", "keyctl", "add_key", "request_key", "open_by_handle_at"):
            self.assertIn(s, H.SECCOMP_BLOCKED_SYSCALLS, s)
        for s in ("unshare", "clone", "clone3", "seccomp", "prctl", "mount", "umount2", "pivot_root"):
            self.assertNotIn(s, H.SECCOMP_BLOCKED_SYSCALLS, s)        # Chrome's own sandbox + the tree/CRIU need these

    def test_seccomp_filter_defaults_allow_and_errnos_the_blocklist(self):
        flt = H.build_seccomp_filter(libseccomp=FakeSeccomp)
        self.assertEqual(flt.defaction, FakeSeccomp.ALLOW)
        blocked = {name for action, name in flt.rules}
        self.assertEqual(blocked, set(H.SECCOMP_BLOCKED_SYSCALLS))
        self.assertTrue(all(action == ("ERRNO", H.SECCOMP_ERRNO) for action, _ in flt.rules))

    def test_capability_allowlist_drops_the_dangerous_caps_and_keeps_bringup_caps(self):
        drop = set(H.caps_to_drop())
        self.assertIn(H._CAP_NUM["CAP_SYS_MODULE"], drop)
        self.assertIn(H._CAP_NUM["CAP_SYS_BOOT"], drop)             # kexec
        self.assertIn(H._CAP_NUM["CAP_SYS_RAWIO"], drop)
        self.assertIn(H._CAP_NUM["CAP_BPF"], drop)
        self.assertNotIn(H._CAP_NUM["CAP_SYS_ADMIN"], drop)        # mounts/pivot_root during bring-up
        self.assertNotIn(H._CAP_NUM["CAP_MKNOD"], drop)           # private /dev nodes

    def test_metadata_firewall_blocks_the_endpoint_and_host_local(self):
        rs = H.metadata_nft_ruleset()
        self.assertIn("169.254.0.0/16", rs)                        # covers 169.254.169.254
        self.assertIn("127.0.0.0/8", rs)
        self.assertIn("drop", rs)
        self.assertIn("oifname \"lo\" accept", rs)                 # the slot's own loopback still works
        self.assertEqual(H.metadata_nft_argv(), ("nft", "-f", "-"))

    def test_apply_runtime_hardening_sets_nnp_drops_caps_then_loads_seccomp(self):
        calls = []
        def fake_prctl(op, *a):
            calls.append((op, a[0] if a else None))
            return 0
        flt_holder = {}
        orig = H.build_seccomp_filter
        H.build_seccomp_filter = lambda libseccomp=None: flt_holder.setdefault("f", FakeSeccomp.SyscallFilter(FakeSeccomp.ALLOW))
        try:
            H.apply_runtime_hardening(prctl=fake_prctl)
        finally:
            H.build_seccomp_filter = orig
        ops = [op for op, _ in calls]
        self.assertEqual(ops[0], H.PR_SET_NO_NEW_PRIVS)                    # no_new_privs first
        self.assertTrue(all(op == H.PR_CAPBSET_DROP for op in ops[1:]))   # then every cap drop
        dropped = {arg for op, arg in calls if op == H.PR_CAPBSET_DROP}
        self.assertEqual(dropped, set(H.caps_to_drop()))
        self.assertTrue(flt_holder["f"].loaded)                           # seccomp loaded last


class UsernsSlotTests(unittest.TestCase):
    """What a slot in its own user namespace needs from the host, found by the app-compat regression of the hardening (2026-10-30):
    the pre-fix tree mknod()ed its devices (EPERM: a user-namespace tmpfs is SB_I_NODEV), read its plan from a 0700 root dir (EACCES),
    ran on a lower layer whose files were all unmapped ("nobody"), loaded nft after the bounding-set drop (no CAP_NET_ADMIN), and
    restored on a trusted.overlay root although the slot's own overlay used user.overlay xattrs."""

    DEV = ("null", "zero", "full", "random", "urandom", "tty")

    def test_userns_plan_binds_the_host_device_nodes_and_makes_none(self):
        plan, _ = plan_for()
        plan["userns"] = {"base": 100000, "count": 65536}
        steps = tr.mount_steps(plan)
        m = plan["merged"]
        self.assertFalse([s for s in steps if s[0] == "mknod"])
        dev_tmpfs = steps.index(("run", ["mount", "-t", "tmpfs", "-o", "mode=755,size=64m", "tmpfs", f"{m}/dev"]))
        for n in self.DEV:
            t = steps.index(("touch", f"{m}/dev/{n}"))
            b = steps.index(("run", ["mount", "--bind", f"/dev/{n}", f"{m}/dev/{n}"]))
            self.assertLess(dev_tmpfs, t)
            self.assertLess(t, b)
        binds = [s for s in steps if s[0] == "run" and s[1][:2] == ["mount", "--bind"]]
        self.assertEqual(sorted(b[1][2] for b in binds), sorted(f"/dev/{n}" for n in self.DEV))     # nothing else from the host
        plan.pop("userns")
        self.assertEqual(len([s for s in tr.mount_steps(plan) if s[0] == "mknod"]), 6)                # without a userns: native nodes

    def test_the_hardened_profile_is_the_default_and_only_an_explicit_false_turns_it_off(self):
        self.assertTrue(H.enabled({}))
        for v in (True, None, 0, "false", "no"):
            self.assertTrue(H.enabled({"harden": v}), v)               # only the JSON value false is the trusted-only opt-out
        self.assertFalse(H.enabled({"harden": False}))

    def test_the_trusted_only_opt_out_launches_without_a_user_namespace_and_skips_the_lockdown(self):
        on = L.launch_argv("M", "P", "LOG", 4, 4096, 3)
        off = L.launch_argv("M", "P", "LOG", 4, 4096, 3, harden=False)
        self.assertIn("--user", on)
        self.assertNotIn("--user", off)
        self.assertNotIn("--map-users", off)
        self.assertEqual([x for x in on if x not in H.userns_unshare_flags(3)], list(off))
        plan, _ = plan_for()
        plan["harden"] = False
        ops = StageOps()
        tr.bring_up(plan, ops, "M1", getpid=lambda: 1)
        kinds = [e[0] for e in ops.events]
        self.assertNotIn("metadata", kinds)
        self.assertNotIn("harden", kinds)
        plan["harden"] = True
        ops = StageOps()
        tr.bring_up(plan, ops, "M1", getpid=lambda: 1)
        self.assertEqual([e[0] for e in ops.events][:4], ["setsid", "metadata", "harden", "source"])

    def test_the_mount_contract_accepts_exactly_the_device_binds(self):
        base = ("1 0 0:1 / / rw - overlay overlay rw\n2 1 0:2 / /dev rw - tmpfs tmpfs rw\n")
        ok = base + "".join(f"{10 + i} 2 0:5 /{n} /dev/{n} rw - devtmpfs udev rw\n" for i, n in enumerate(self.DEV))
        self.assertEqual(cr.mount_contract(ok), [])
        for bad in ("30 2 0:5 /sda /dev/sda rw - devtmpfs udev rw\n",          # any other host device
                    "30 2 0:5 /sda /dev/null rw - devtmpfs udev rw\n",         # a different node under an allowed name
                    "30 2 0:5 / /dev/null rw - devtmpfs udev rw\n",            # the whole host devtmpfs
                    "30 2 0:9 /null /dev/null rw - tmpfs tmpfs rw\n",          # a bind of something that is not the host's devtmpfs
                    "30 1 0:5 /null /null rw - devtmpfs udev rw\n"):           # an allowed node somewhere else
            self.assertTrue(cr.mount_contract(base + bad), bad)

    def test_dump_and_restore_name_the_same_external_keys(self):
        # the externals come from the slot's own mount table (criu_root.dump_externals / restore_externals)
        text = "21 1 0:30 / / rw - overlay overlay rw\n" + "".join(
            f"{40 + i} 21 0:2 /{n} /dev/{n} rw,nosuid - devtmpfs udev rw\n" for i, n in enumerate(self.DEV))
        rows = [[m["mountpoint"], m["fstype"], m["root"]] for m in cr.parse_mountinfo(text)]
        d = L.dump_argv(7, "/i", "/l", externals=cr.dump_externals(text))
        r = cr.restore_argv("m", 1, 4, 4096, "/i", "/p", "/l", "/img", "/u", "/w", "/r", externals=cr.restore_externals(rows, "/run/drums/procview"))
        dk = {a[a.index("]:") + 2:]: a[4:a.index("]")] for a in (d[i + 1] for i, x in enumerate(d) if x == "--external")}
        rk = {a[4:a.index("]")]: a[a.index("]:") + 2:] for a in (r[i + 1] for i, x in enumerate(r) if x == "--external")}
        self.assertEqual(set(dk), set(rk))
        for key, mnt in dk.items():
            self.assertEqual(rk[key], mnt)                         # restored from the host node of the same name
            self.assertIn(mnt[len("/dev/"):], self.DEV)
        self.assertEqual(len(dk), 6)

    def test_userns_restore_root_reads_user_overlay_xattrs(self):
        self.assertEqual(cr.overlay_options("/l", "/u", "/w"), "lowerdir=/l,upperdir=/u,workdir=/w")
        self.assertEqual(cr.overlay_options("/l", "/u", "/w", userxattr=True), "lowerdir=/l,upperdir=/u,workdir=/w,userxattr")
        r = cr.restore_argv("m", 1, 4, 4096, "/i", "/p", "/l", "/img", "/u", "/w", "/r", userxattr=True)
        self.assertIn("lowerdir=/img,upperdir=/u,workdir=/w,userxattr", r)

    def test_idmapped_lower_path_is_per_image_and_base(self):
        a, b = H.idmapped_lower_path("/mnt/os", 100000), H.idmapped_lower_path("/mnt/os", 165536)
        self.assertNotEqual(a, b)
        self.assertTrue(a.startswith(H.IDMAP_ROOT + "/"))
        self.assertNotEqual(a, H.idmapped_lower_path("/mnt/other", 100000))

    @unittest.skipUnless(sys.platform.startswith("linux") and os.geteuid() == 0, "needs root on Linux (chown, mounts)")
    def test_host_preparation_gives_the_mapped_root_only_its_own_slot(self):
        with tempfile.TemporaryDirectory() as d:
            root, slot = os.path.join(d, "state"), os.path.join(d, "state", "slot3")
            up, wk, mg = (os.path.join(slot, n) for n in ("upper", "work", "merged"))
            for x in (up, wk, mg):
                os.makedirs(x)
            os.chmod(root, 0o700)
            os.chmod(slot, 0o700)
            plan = os.path.join(slot, "service-plan.json")
            open(plan, "w").write("{}")
            os.makedirs(os.path.join(up, "usr/lib"))
            open(os.path.join(up, "usr/lib/drv.so"), "w").close()
            os.chown(os.path.join(up, "usr/lib/drv.so"), 0, 5)
            H.prepare_slot_access(root, slot, plan, (up, wk, mg), 300000)
            self.assertEqual(stat.S_IMODE(os.stat(root).st_mode), 0o711)
            st = os.stat(slot)
            self.assertEqual((st.st_uid, st.st_gid, stat.S_IMODE(st.st_mode)), (0, 300000, 0o710))
            st = os.stat(plan)
            self.assertEqual((st.st_uid, st.st_gid, stat.S_IMODE(st.st_mode)), (0, 300000, 0o640))
            for x in (up, wk, mg):
                self.assertEqual((os.stat(x).st_uid, os.stat(x).st_gid), (300000, 300000))
            H.shift_owner(up, 300000)
            st = os.lstat(os.path.join(up, "usr/lib/drv.so"))
            self.assertEqual((st.st_uid, st.st_gid), (300000, 300005))          # in-image owner kept, moved into the slot's range
            self.assertEqual(os.stat(os.path.join(up, "usr")).st_uid, 300000)
            H.shift_owner(up, 300000)                                              # idempotent
            self.assertEqual(os.lstat(os.path.join(up, "usr/lib/drv.so")).st_gid, 300005)

    @unittest.skipUnless(sys.platform.startswith("linux") and os.geteuid() == 0, "needs root on Linux (idmapped mounts)")
    def test_idmapped_lower_shows_image_ids_shifted_and_is_read_only(self):
        with tempfile.TemporaryDirectory() as d:
            img = os.path.join(d, "img")
            os.makedirs(os.path.join(img, "home/user"))
            os.chown(os.path.join(img, "home/user"), 1000, 1000)
            subprocess.run(["mount", "-t", "tmpfs", "tmpfs", img], check=True)        # a fresh superblock (idmap needs FS_ALLOW_IDMAP)
            try:
                os.makedirs(os.path.join(img, "home/user"))
                os.chown(os.path.join(img, "home/user"), 1000, 1000)
                subprocess.run(["mount", "-o", "remount,ro", img], check=True)
                old = H.IDMAP_ROOT
                H.IDMAP_ROOT = os.path.join(d, "idmap")
                try:
                    p = H.ensure_idmapped_lower(img, 400000)
                    self.assertEqual(H.ensure_idmapped_lower(img, 400000), p)           # reused, not stacked
                    st = os.stat(os.path.join(p, "home/user"))
                    self.assertEqual((st.st_uid, st.st_gid), (401000, 401000))
                    self.assertEqual(os.stat(p).st_uid, 400000)
                    with self.assertRaises(OSError):
                        open(os.path.join(p, "x"), "w")
                finally:
                    subprocess.run(["umount", p], check=False)
                    H.IDMAP_ROOT = old
            finally:
                subprocess.run(["umount", img], check=False)

    def test_provision_runs_the_slot_on_the_idmapped_lower_with_its_own_access(self):
        fake = FakeUserns()
        fake.idmapped_lower = lambda lower, base: (fake.calls.append(("idmapped_lower", lower, base)), f"/run/idm-{base}")[1]
        a, world, ex = ArmEnv(self).arm(userns_host=fake)
        if True:
            a.provision(0)
            p = L.slot_paths(a.cfg["state_dir"], 0)
            plan = json.load(open(p["plan"]))
            self.assertEqual(plan["userns"], {"base": H.subid_base(0), "count": H.SUBID_COUNT})
            self.assertEqual(plan["lower"], f"/run/idm-{H.subid_base(0)}")
            kinds = [c[0] for c in fake.calls]
            self.assertEqual(kinds, ["idmapped_lower", "shift_owner", "prepare_access"])
            self.assertEqual(fake.calls[1][1:], (p["upper"], H.subid_base(0)))
            self.assertEqual(fake.calls[2][2:], (p["dir"], p["plan"], (p["upper"], p["work"], p["merged"]), H.subid_base(0)))

    def test_the_trusted_only_opt_out_provisions_on_the_image_itself_with_no_userns_preparation(self):
        fake = FakeUserns()
        a, world, ex = ArmEnv(self).arm(userns_host=fake)
        a.cfg["harden"] = False
        a.provision(0)
        p = L.slot_paths(a.cfg["state_dir"], 0)
        plan = json.load(open(p["plan"]))
        self.assertNotIn("userns", plan)
        self.assertIs(plan["harden"], False)
        self.assertEqual(plan["lower"], a.cfg["lower"])
        self.assertEqual(fake.calls, [])
        launched = [c for c in ex.calls if "unshare" in c]
        self.assertTrue(launched)
        self.assertFalse([c for c in launched if "--user" in c])


if __name__ == "__main__":
    unittest.main()


FIX = os.path.join(os.path.dirname(os.path.abspath(__file__)), "fixtures", "session-env")


class SessionLoginTests(unittest.TestCase):
    """The image's login, reproduced in the slot (fix/session-fidelity, 2026-10-30)."""

    def test_the_inspection_reads_the_images_login_from_its_files(self):
        r = make_rootfs(tmp(self))
        lg = ri.inspect_rootfs(r)["login"]
        self.assertEqual(lg["pam_env_files"], ["/etc/environment", "/etc/default/locale"])
        self.assertEqual(lg["locale"], {"LANG": "en_HK.UTF-8", "LANGUAGE": "en_HK:en"})        # quotes and comment dropped
        self.assertEqual(lg["pam_env"]["PATH"], "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/usr/games:/snap/bin")
        self.assertEqual(lg["keyring"], "/usr/bin/gnome-keyring-daemon")                       # pam_gnome_keyring auto_start
        # systemd order; a non-executable generator is not run; an empty file in /etc masks /usr/lib's of the same name
        self.assertEqual(lg["user_generators"], ["/usr/lib/systemd/user-environment-generators/30-env-d"])
        self.assertEqual(lg["system_generators"], ["/usr/lib/systemd/system-environment-generators/snapd-env-generator"])
        self.assertEqual([c["exec"] for c in lg["components"]], ["/usr/libexec/gsd-xsettings", "/usr/libexec/gsd-media-keys"])
        self.assertEqual(lg["session_manager_env"], {"XDG_MENU_PREFIX": "gnome-", "GNOME_DESKTOP_SESSION_ID": "this-is-deprecated"})
        self.assertTrue(lg["gdm_sets_username"] and lg["gdm_sets_gdmsession"])
        self.assertEqual((lg["xsession"], lg["unsupported"]), ("/etc/gdm3/Xsession", []))

    def test_what_cannot_be_reproduced_refuses_the_slot(self):
        r = make_rootfs(tmp(self))
        open(os.path.join(r, "etc/security/pam_env.conf"), "a").write("FOO DEFAULT=bar\n")
        plan, _ = plan_for(r)
        with self.assertRaises(tr.PlanError) as cm:
            tr.services_plan(plan)
        self.assertIn("pam_env.conf", str(cm.exception))

    def test_without_login_facts_the_old_session_is_kept_as_it_was(self):
        r = make_rootfs(tmp(self), login=False)
        plan, _ = plan_for(r)
        with self.assertRaises(tr.PlanError):                               # an image whose login is missing parts: refused
            tr.services_plan(plan)
        plan["inspection"]["login"] = None                                  # no login facts at all (display_only, OCI reference)
        self.assertEqual([s["name"] for s in tr.services_plan(plan)], ["system_dbus", "dbus", "display", "desktop", "server"])
        self.assertEqual(tr.user_env(plan)["LANG"], "C.UTF-8")

    def test_the_server_unit_environment_is_parsed_with_systemd_quoting(self):
        r = make_rootfs(tmp(self))
        open(os.path.join(r, "etc/systemd/system/osworld_server.service"), "w").write(
            '[Service]\nExecStart=/usr/bin/python3 /home/user/server/main.py\nEnvironment="DISPLAY=:0"\n'
            'Environment="DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus" A=1\n')
        plan, _ = plan_for(r)
        self.assertEqual(plan["server"]["env"], {"DISPLAY": ":0", "DBUS_SESSION_BUS_ADDRESS": "unix:path=/run/user/1000/bus",
                                                 "A": "1"})                 # was {'"DISPLAY': ':0', ...}

    def test_run_login_runs_the_images_own_scripts_in_order(self):
        if os.geteuid() == 0:
            self.skipTest("runs the fake image's scripts as the current user")
        r = make_rootfs(tmp(self))
        plan, _ = plan_for(r)
        spec = next(s for s in tr.services_plan(plan) if s["name"] == "login")
        for k in ("keyring", "xsession", "activation"):
            spec[k] = r + spec[k]
        spec["generators"] = [r + g for g in spec["generators"]]
        open(r + "/usr/bin/gnome-keyring-daemon", "w").write("#!/bin/sh\necho SSH_AUTH_SOCK=/run/user/1000/keyring/ssh\n")
        rt = tmp(self)
        envs = {"session_base": dict(tr.user_env(plan), XDG_RUNTIME_DIR=rt, HOME=rt)}
        ops = sv.Ops(tmp(self))
        u = {"uid": os.getuid(), "gid": os.getgid(), "groups": []}
        real = ops.capture

        def as_me(name, argv, env, uid, gid, groups=(), timeout=60, stdin=b""):      # no privilege drop when already that user
            p = subprocess.run(argv, env=env, input=stdin, capture_output=True, timeout=timeout)
            return p.returncode, p.stdout
        ops.capture = as_me
        env = sv.run_login(spec, envs, ops, u)
        self.assertEqual(env["SSH_AUTH_SOCK"], "/run/user/1000/keyring/ssh")                  # from the keyring step
        self.assertTrue(env["PATH"].endswith(":/snap/bin"))                                  # from the user generator
        self.assertEqual(env["FROM_XSESSION"], "1")                                          # through the X session script
        self.assertEqual((env["LANG"], env["XDG_MENU_PREFIX"]), ("en_HK.UTF-8", "gnome-"))
        self.assertFalse(os.path.exists(os.path.join(rt, ".drums-login-capture")))           # nothing left in the runtime dir
        self.assertIs(envs["session"], env)
        self.assertTrue(callable(real))

    def test_the_fixed_session_matches_the_vm_login_and_the_g1_session_did_not(self):
        from sharedos_rootfs import fidelity as fd
        rd = lambda n: fd.parse(open(os.path.join(FIX, n)).read())
        for proc in ("gnome-shell", "server"):
            d = fd.differences(rd(f"vm-{proc}.env"), rd(f"drums-fixed-{proc}.env"))
            self.assertEqual(d["unexpected"], [], proc)
        old = fd.differences(rd("vm-gnome-shell.env"), rd("drums-g1-gnome-shell.env"))
        names = {r[0] for r in old["unexpected"]}
        self.assertLessEqual({"LANG", "LANGUAGE", "XDG_DATA_DIRS", "GTK_IM_MODULE", "XMODIFIERS", "PATH", '"DISPLAY'}, names)
        srv = fd.differences(rd("vm-server.env"), rd("drums-g1-server.env"))
        self.assertIn("LANG", {r[0] for r in srv["unexpected"]})
