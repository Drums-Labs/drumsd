"""The per-env memory/CPU view (drumsd procview) in the slot tree: read-only binds made by stage A, allowed by the mount
contract only in their exact form, and named to CRIU as external mounts at dump and restore."""
import os
import sys
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
from sharedos_rootfs import criu_root as cr  # noqa: E402
from sharedos_rootfs import launch as L  # noqa: E402
from sharedos_rootfs import tree as tr  # noqa: E402

BASE = ("636 600 0:52 / / rw,relatime - overlay overlay rw,lowerdir=/mnt/osworld,upperdir=/s/upper,workdir=/s/work\n"
        "637 636 0:53 / /proc rw,nosuid - proc proc rw\n"
        "638 636 0:54 / /sys ro,nosuid - sysfs sysfs ro\n")


def pv_line(i, name, target, opts="ro,nosuid,nodev,noexec,relatime"):
    return f"{700 + i} 637 0:60 /{name} {target} {opts} - fuse.drums-procview drums-procview ro,user_id=0,group_id=0,default_permissions,allow_other\n"


ALL = BASE + "".join(pv_line(i, n, t) for i, (n, t) in enumerate(sorted(cr.PROCVIEW_FILES.items())))


class Contract(unittest.TestCase):
    def test_exact_view_binds_are_allowed(self):
        self.assertEqual(cr.mount_contract(ALL), [])

    def test_a_view_file_bound_elsewhere_or_writable_is_refused(self):
        self.assertTrue(cr.mount_contract(BASE + pv_line(1, "meminfo", "/etc/passwd")))
        self.assertTrue(cr.mount_contract(BASE + pv_line(1, "meminfo", "/proc/cpuinfo")))
        self.assertTrue(cr.mount_contract(BASE + pv_line(1, "meminfo", "/proc/meminfo", "rw,nosuid")))
        # another FUSE file system is still an external mount the restore has no mapping for
        self.assertTrue(cr.mount_contract(BASE + "750 637 0:61 / /proc/meminfo ro - fuse.lxcfs lxcfs ro\n"))

    def test_without_the_view_nothing_changes(self):
        self.assertEqual(cr.mount_contract(BASE), [])
        self.assertEqual(cr.procview_dump_externals(BASE), ())


class Externals(unittest.TestCase):
    def test_dump_names_each_bind_and_restore_maps_it_back(self):
        d = cr.procview_dump_externals(ALL)
        self.assertEqual(len(d), 2 * len(cr.PROCVIEW_FILES))
        self.assertIn("mnt[/proc/meminfo]:drums-pv-meminfo", d)
        self.assertIn("mnt[/sys/devices/system/cpu/online]:drums-pv-cpu_online", d)
        rows = sorted([m["mountpoint"], m["fstype"], m["root"]] for m in cr.parse_mountinfo(ALL))
        r = cr.procview_restore_externals(rows, "/run/drums/procview/")
        self.assertIn("mnt[drums-pv-meminfo]:/run/drums/procview/meminfo", r)
        self.assertEqual(len(r), len(d))
        self.assertEqual(cr.procview_restore_externals([["/", "overlay", "/"]], "/x"), ())

    def test_dump_and_restore_argv_carry_them(self):
        ext = ("--external", "mnt[/proc/meminfo]:drums-pv-meminfo")
        a = L.dump_argv(7, "/i", "/l", leave_running=True, externals=ext)
        self.assertEqual(a[a.index("--external") + 1], "mnt[/proc/meminfo]:drums-pv-meminfo")
        r = cr.restore_argv("m", 1, 4, 4096, "/i", "/p", "/l", "/lo", "/u", "/w", "/c", externals=("--external", "mnt[drums-pv-meminfo]:/v/meminfo"))
        self.assertIn("mnt[drums-pv-meminfo]:/v/meminfo", r)
        self.assertNotIn("--external", L.dump_argv(7, "/i", "/l"))


class TreeSteps(unittest.TestCase):
    PLAN = {"merged": "/s/merged", "lower": "/lo", "upper": "/s/upper", "work": "/s/work", "tty_gid": 5}

    def test_binds_follow_proc_and_sys_and_only_with_a_view(self):
        steps = tr.mount_steps(dict(self.PLAN, procview={"dir": "/run/drums/procview", "files": cr.PROCVIEW_FILES}))
        names = [s[0] for s in steps]
        binds = [s for s in steps if s[0] == "bind_view"]
        self.assertEqual(len(binds), len(cr.PROCVIEW_FILES))
        self.assertIn(("bind_view", "/run/drums/procview/meminfo", "/s/merged/proc/meminfo"), binds)
        self.assertGreater(names.index("bind_view"), max(i for i, s in enumerate(steps) if s[0] == "run" and "sysfs" in s[1]))
        self.assertNotIn("bind_view", [s[0] for s in tr.mount_steps(dict(self.PLAN))])
        self.assertNotIn("bind_view", [s[0] for s in tr.mount_steps(dict(self.PLAN, procview=None))])

    def test_a_source_that_is_not_the_fuse_view_is_refused(self):
        with self.assertRaises(tr.PlanError):
            tr.Ops().bind_view("/etc/hostname", "/tmp/x")


if __name__ == "__main__":
    unittest.main()
