"""capture.py (post-dump upper copy for --leave-running) and pageshare.py (template refusal -> private restore fallback) with fakes.
These prove the harness logic only; the live gates are the evidence that CRIU checkpoints and restores the real tree."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))

from sharedos_rootfs import capture as cap  # noqa: E402
from sharedos_rootfs import pageshare as ps  # noqa: E402
from sharedos_rootfs.arm import ArmError  # noqa: E402


def tmp(tc):
    d = tempfile.mkdtemp()
    tc.addCleanup(shutil.rmtree, d, True)
    return d


class PostDumpScriptTests(unittest.TestCase):
    def setUp(self):
        self.d = tmp(self)
        self.upper = os.path.join(self.d, "slotP", "upper")
        os.makedirs(os.path.join(self.upper, "home", "user", ".config", "dconf"))
        with open(os.path.join(self.upper, "home", "user", ".config", "dconf", "user"), "w") as f:
            f.write("db")
        self.q = os.path.join(self.d, "slotQ")
        os.makedirs(self.q)
        self.script = cap.post_dump_script(self.q, self.upper)

    def run_action(self, action):
        return subprocess.run([self.script], env={**os.environ, "CRTOOLS_SCRIPT_ACTION": action}, capture_output=True, text=True)

    def test_copies_the_upper_layer_only_at_post_dump_and_records_the_copy_time(self):
        for other in ("pre-dump", "network-lock", "network-unlock", "pre-resume"):
            self.assertEqual(self.run_action(other).returncode, 0)
            self.assertFalse(os.path.exists(os.path.join(self.q, "upper.snap")))
        r = self.run_action("post-dump")
        self.assertEqual(r.returncode, 0, r.stderr)
        with open(os.path.join(self.q, "upper.snap", "home", "user", ".config", "dconf", "user")) as f:
            self.assertEqual(f.read(), "db")
        rec = json.load(open(os.path.join(self.q, "post-dump.json")))
        self.assertEqual(rec["action"], "post-dump")
        self.assertGreaterEqual(rec["copy_ns"], 0)

    def test_a_failed_copy_fails_the_action_so_criu_does_not_report_a_good_dump(self):
        shutil.rmtree(self.upper)
        self.assertNotEqual(self.run_action("post-dump").returncode, 0)
        self.assertFalse(os.path.exists(os.path.join(self.q, "post-dump.json")))

    def test_the_script_is_created_once_and_not_world_writable(self):
        self.assertEqual(os.stat(self.script).st_mode & 0o777, 0o700)
        with self.assertRaises(FileExistsError):
            cap.post_dump_script(self.q, self.upper)

    def test_paths_with_shell_metacharacters_are_quoted(self):
        q2 = os.path.join(self.d, "q $(touch pwned) ;")
        os.makedirs(q2)
        s = cap.post_dump_script(q2, self.upper)
        r = subprocess.run([s], env={**os.environ, "CRTOOLS_SCRIPT_ACTION": "post-dump"}, capture_output=True, text=True, cwd=self.d)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertFalse(os.path.exists(os.path.join(self.d, "pwned")))
        self.assertTrue(os.path.isdir(os.path.join(q2, "upper.snap")))


class CgroupKillBackstopTests(unittest.TestCase):
    """branch.cgroup_kill_backstop: a tree that survives the pid-by-pid SIGKILL is killed through its own cgroup, or fails closed."""

    def setUp(self):
        from sharedos_rootfs import branch as br
        self.br = br
        self.d = tmp(self)
        self.proc, self.cg = os.path.join(self.d, "proc"), os.path.join(self.d, "cg")
        self.host = br.ScopedHost("rootfs-ab-3", proc=self.proc)

    def task(self, pid, cgpath, populated=True):
        os.makedirs(os.path.join(self.proc, str(pid)), exist_ok=True)
        open(os.path.join(self.proc, str(pid), "cgroup"), "w").write(f"0::{cgpath}\n")
        d = os.path.join(self.cg, cgpath.lstrip("/"))
        os.makedirs(d, exist_ok=True)
        open(os.path.join(d, "cgroup.events"), "w").write(f"populated {1 if populated else 0}\nfrozen 0\n")
        return d

    def test_survivor_is_killed_through_its_own_cgroup(self):
        # exp3 criufix 08:54:43Z: {'pid': 908459, 'why': 'still alive after SIGKILL'}
        d = self.task(908459, "/system.slice/drumsbench-rootfs-ab-3-r7.scope")

        def kernel(_s):                      # the kernel empties the cgroup once cgroup.kill was written
            if os.path.exists(os.path.join(d, "cgroup.kill")):
                open(os.path.join(d, "cgroup.events"), "w").write("populated 0\nfrozen 0\n")
        rep = self.br.cgroup_kill_backstop(self.host, {"failures": [{"pid": 908459, "why": "still alive after SIGKILL"}], "remaining": []},
                                           cg_root=self.cg, wait_s=5, sleep=kernel)
        self.assertTrue(rep["ok"], rep)
        self.assertEqual(rep["killed"], ["/system.slice/drumsbench-rootfs-ab-3-r7.scope"])
        self.assertEqual(open(os.path.join(d, "cgroup.kill")).read(), "1")

    def test_pool_scope_of_a_pageshare_slot_is_accepted(self):
        from sharedos_rootfs import pageshare as ps
        host = ps.PoolScopedHost("rootfs-ab-3", proc=self.proc)
        d = self.task(42, "/system.slice/drumsbench-pool.scope/drumsbench-rootfs-ab-3-pool.scope")
        rep = self.br.cgroup_kill_backstop(host, {"failures": [{"pid": 42}], "remaining": []}, cg_root=self.cg, wait_s=1,
                                           sleep=lambda s: open(os.path.join(d, "cgroup.events"), "w").write("populated 0\n"))
        self.assertTrue(rep["ok"], rep)

    def test_a_task_that_never_leaves_fails_closed_within_the_bound(self):
        self.task(7, "/system.slice/drumsbench-rootfs-ab-3-r1.scope")          # e.g. stuck in D state: populated stays 1
        t = {"now": 0.0}

        def clock():
            return t["now"]

        def sleep(s):
            t["now"] += s
        rep = self.br.cgroup_kill_backstop(self.host, {"failures": [{"pid": 7}], "remaining": []}, cg_root=self.cg, wait_s=2,
                                           sleep=sleep, clock=clock)
        self.assertFalse(rep["ok"])
        self.assertEqual(rep["still_populated"], ["/system.slice/drumsbench-rootfs-ab-3-r1.scope"])
        self.assertLessEqual(rep["seconds"], 2.2)

    def test_another_slots_or_the_hosts_cgroup_is_never_killed(self):
        other = self.task(8, "/system.slice/drumsbench-rootfs-ab-30-r1.scope")   # prefix collision: slot 30, not slot 3
        mine = self.task(9, "/user.slice/session-1.scope")
        rep = self.br.cgroup_kill_backstop(self.host, {"failures": [{"pid": 8}, {"pid": 9}], "remaining": []}, cg_root=self.cg, wait_s=0.2,
                                           sleep=lambda s: None)
        self.assertFalse(rep["ok"])
        self.assertEqual(rep["killed"], [])
        self.assertFalse(os.path.exists(os.path.join(other, "cgroup.kill")))
        self.assertFalse(os.path.exists(os.path.join(mine, "cgroup.kill")))
        self.assertEqual(len(rep["foreign"]), 2)


class FailedCaptureTests(unittest.TestCase):
    """CaptureArm.capture: a failed capture must not leave the capture slot Q behind (it blocked every later capture of the parent)."""

    def setUp(self):
        self.d = tmp(self)
        self.arm = cap.CaptureArm.__new__(cap.CaptureArm)
        self.arm.cfg = {"state_dir": self.d}
        self.q = os.path.join(self.d, "slot509")

    def failing(self, slot, into, *a):
        os.makedirs(os.path.join(self.q, "images"))
        open(os.path.join(self.q, "dump.log"), "w").write("Error (criu/cr-dump.c:1): x\n")
        open(os.path.join(self.q, "images", "pages-1.img"), "w").write("x" * 100)
        raise ArmError("criu dump (leaves the tree running; upper copied at post-dump) failed rc=1 error=None stderr=''")

    def test_a_failed_dump_frees_the_capture_slot_and_keeps_its_logs(self):
        self.arm._capture = self.failing
        with self.assertRaises(ArmError) as cm:
            self.arm.capture(8, 509, leave_running=True)
        self.assertIn("criu dump", str(cm.exception))
        self.assertIn("slot 509 is free again", str(cm.exception))
        self.assertFalse(os.path.lexists(self.q))                      # the next capture of this parent can create Q again
        aside = [n for n in os.listdir(self.d) if n.startswith("slot509.failed-capture-")]
        self.assertEqual(len(aside), 1)
        self.assertTrue(os.path.isfile(os.path.join(self.d, aside[0], "dump.log")))       # evidence kept
        self.assertFalse(os.path.exists(os.path.join(self.d, aside[0], "images")))        # bulk dropped
        self.arm._capture = lambda *a: {"ok": True}
        self.assertEqual(self.arm.capture(8, 509), {"ok": True})

    def test_a_q_that_existed_before_the_call_is_never_moved(self):
        os.makedirs(self.q)
        self.arm._capture = lambda *a: (_ for _ in ()).throw(ArmError("slot 509 already exists"))
        with self.assertRaises(ArmError):
            self.arm.capture(8, 509)
        self.assertTrue(os.path.isdir(self.q))
        self.assertEqual([n for n in os.listdir(self.d) if "failed-capture" in n], [])


class FallbackTests(unittest.TestCase):
    """PageshareArm.restore: a template refusal BEFORE any task ran falls back to a private restore and is recorded; nothing else is retried."""

    def setUp(self):
        self.d = tmp(self)
        self.slotdir = os.path.join(self.d, "slot7")
        os.makedirs(self.slotdir)
        self.ps = {"mode": "template", "socket": "/x.sock", "criu": None}
        self.records = []
        arm = ps.PageshareArm.__new__(ps.PageshareArm)
        arm.cfg = {"state_dir": self.d}
        arm._ps = lambda slot: dict(self.ps)
        arm.use_slot = lambda slot, m=None: None

        def record(slot, **kw):
            self.records.append(kw)
            if "pageshare" in kw:
                self.ps = kw["pageshare"]
            return {"slot": slot, **kw}
        arm._record = record
        self.arm = arm

    def log(self, text):
        with open(os.path.join(self.slotdir, "restore-restore-1.log"), "w") as f:
            f.write(text)

    def test_needs_a_premap_refusal_falls_back_to_a_private_restore_in_the_same_cgroup(self):
        calls = []

        def base(slf, slot):
            calls.append(self.ps["mode"])
            if self.ps["mode"] == "template":
                self.log("(00.3) Error (criu/mem.c:1587): Task 52: VMA 0x7ffd-0x7ffe needs a premap; not supported with a template\n")
                raise ArmError("criu restore failed rc=1")
            return {"state": "running"}
        with mock.patch.object(ps.BranchArm, "restore", base):
            rep = self.arm.restore(7)
        self.assertEqual(calls, ["template", "pool"])
        self.assertEqual(self.ps["mode"], "pool")
        fb = rep["last_restore_fallback"]
        self.assertIn("needs a premap", fb["refusal"][0])
        self.assertEqual(self.ps["template_refused"], fb)

    def test_any_other_restore_failure_is_raised_and_not_retried(self):
        calls = []

        def base(slf, slot):
            calls.append(self.ps["mode"])
            self.log("(00.3) Error (criu/cr-restore.c:2100): Unable to detach 71: No such process\n")
            raise ArmError("criu restore failed rc=1")
        with mock.patch.object(ps.BranchArm, "restore", base):
            with self.assertRaises(ArmError):
                self.arm.restore(7)
        self.assertEqual(calls, ["template"])
        self.assertEqual(self.records, [])

    def test_a_stale_log_from_an_earlier_restore_is_not_read_as_this_refusal(self):
        self.log("(00.3) Task 52: VMA 0x1-0x2 needs a premap; not supported with a template\n")
        old = os.path.join(self.slotdir, "restore-restore-1.log")
        os.utime(old, (1, 1))

        def base(slf, slot):
            raise ArmError("criu restore failed rc=1 (no new log)")
        with mock.patch.object(ps.BranchArm, "restore", base):
            with self.assertRaises(ArmError):
                self.arm.restore(7)

    def test_pool_mode_restores_are_never_rewritten(self):
        self.ps["mode"] = "pool"
        with mock.patch.object(ps.BranchArm, "restore", lambda slf, slot: {"state": "running", "mode": self.ps["mode"]}):
            self.assertEqual(self.arm.restore(7)["mode"], "pool")
        self.assertEqual(self.records, [])


def make_slot(state_dir, slot, created):
    d = os.path.join(state_dir, f"slot{slot}")
    os.makedirs(d)
    with open(os.path.join(d, ps.SLOT_MARKER), "w") as f:
        json.dump({"slot": slot, "created_epoch": created}, f)
    return d


def replace_slot(state_dir, slot, created):
    """What a state reset and the next run do: the slot directory is deleted and a new one is made at the same path."""
    shutil.rmtree(os.path.join(state_dir, f"slot{slot}"))
    return make_slot(state_dir, slot, created)


class ReplacedSlotTests(unittest.TestCase):
    """h11c-93eabf48 child 17 (userns-appcompat 20261030T2144Z): a `pageshare branch` process of an earlier, aborted run outlived the
    state reset (its client was killed, but the hung restorer kept the client's stdout and stderr). When it woke, its failure path swept
    "slot 17", which by then was the NEW run's slot at the same path with the same pool cgroup, and cgroup.kill killed the new client."""

    def setUp(self):
        self.d = tmp(self)
        make_slot(self.d, 17, 1.0)
        self.sweeps, self.killed = [], []
        self.tree_ok = [True]
        arm = ps.PageshareArm.__new__(ps.PageshareArm)
        arm.cfg = {"state_dir": self.d}
        arm.ph = "proxy-host"
        arm._ps = lambda slot: {"mode": "template", "socket": "/x.sock"}
        arm._marker = lambda slot: f"rootfs-x-{slot}"
        arm._argv_marker = lambda slot: "rootfs-x-0"
        self.arm = arm

        def sweep_marker(host, marker, *a, **kw):
            self.sweeps.append((host, marker))
            ok = True if host == "proxy-host" else self.tree_ok.pop(0)
            return {"marker": marker, "ok": ok, "failures": [] if ok else [{"pid": 5, "why": "still alive after SIGKILL"}]}
        p1 = mock.patch.object(ps.rt, "sweep_marker", sweep_marker)
        p2 = mock.patch.object(ps, "empty_slot_cg", lambda d: self.killed.append(d) or {"left_after_sweep": ["9"], "cgroup_kill": True})
        p1.start(), p2.start()
        self.addCleanup(p1.stop), self.addCleanup(p2.stop)

    def test_the_slot_this_process_took_is_swept_and_its_cgroup_emptied_as_before(self):
        self.arm.use_slot(17)
        rep = self.arm.sweep(17)
        self.assertTrue(rep["ok"])
        self.assertEqual(len(self.sweeps), 2)
        self.assertEqual(self.killed, [ps.pool_cg("rootfs-x-17")])

    def test_a_process_whose_slot_was_replaced_signals_nothing_in_it(self):
        self.arm.use_slot(17)
        replace_slot(self.d, 17, 2.0)
        rep = self.arm.sweep(17)
        self.assertFalse(rep["ok"])
        self.assertIn("replaced", rep)
        self.assertEqual(self.sweeps, [])
        self.assertEqual(self.killed, [])

    def test_a_later_use_slot_does_not_adopt_the_replacement(self):
        self.arm.use_slot(17)
        replace_slot(self.d, 17, 2.0)
        self.arm.use_slot(17)        # restore()'s template fallback calls use_slot again
        self.assertFalse(self.arm.sweep(17)["ok"])
        self.assertEqual(self.killed, [])

    def test_a_slot_made_after_use_slot_is_taken_at_the_next_use_slot(self):
        shutil.rmtree(os.path.join(self.d, "slot17"))
        self.arm.use_slot(17)        # main() of `branch`: the slot does not exist yet
        make_slot(self.d, 17, 3.0)   # branch() makes it, then calls use_slot before the restore
        self.arm.use_slot(17)
        replace_slot(self.d, 17, 4.0)
        self.assertFalse(self.arm.sweep(17)["ok"])
        self.assertEqual(self.killed, [])

    def test_a_process_that_never_took_the_slot_sweeps_what_is_there(self):
        """cleanup walks every slot with one arm and no use_slot: it acts on the slots that exist now."""
        replace_slot(self.d, 17, 2.0)
        self.assertTrue(self.arm.sweep(17)["ok"])
        self.assertEqual(len(self.killed), 1)

    def test_a_slot_replaced_during_the_argv_sweeps_keeps_its_cgroup(self):
        self.arm.use_slot(17)
        real = ps.rt.sweep_marker

        def sweep_then_reset(host, marker, *a, **kw):
            rep = real(host, marker, *a, **kw)
            if host != "proxy-host":
                replace_slot(self.d, 17, 2.0)
            return rep
        with mock.patch.object(ps.rt, "sweep_marker", sweep_then_reset):
            rep = self.arm.sweep(17)
        self.assertFalse(rep["ok"])
        self.assertIn("replaced", rep)
        self.assertEqual(self.killed, [])

    def test_no_logs_are_written_into_a_replaced_slot(self):
        calls = []
        self.arm.use_slot(17)
        replace_slot(self.d, 17, 2.0)
        with mock.patch.object(ps.BranchArm, "_preserve_logs", lambda slf, slot, label: calls.append(label) or "x"):
            self.assertIsNone(self.arm._preserve_logs(17, "restore-1"))
        self.assertEqual(calls, [])

    def test_a_replaced_slot_is_not_read_for_a_template_refusal(self):
        self.arm.use_slot(17)
        records = []
        self.arm._record = lambda slot, **kw: records.append(kw)

        def base(slf, slot):
            d = replace_slot(self.d, 17, 2.0)
            with open(os.path.join(d, "restore-restore-9.log"), "w") as f:
                f.write("Task 52: VMA 0x1-0x2 needs a premap; not supported with a template\n")
            raise ArmError("criu restore failed rc=-15")
        with mock.patch.object(ps.BranchArm, "restore", base):
            with self.assertRaises(ArmError):
                self.arm.restore(17)
        self.assertEqual(records, [])

    def test_the_cgroup_backstop_stops_a_tree_the_argv_sweep_could_not(self):
        """A hung template restore: the restored init got SIGKILL but cannot finish exiting while CRIU's forked restorer (no marker argv,
        so the argv sweep never signals it) holds the tree. The slot's own pool cgroup holds only this slot's processes."""
        self.tree_ok[:] = [False, True]
        self.arm.use_slot(17)
        rep = self.arm.sweep(17)
        self.assertEqual(self.killed, [ps.pool_cg("rootfs-x-17")])
        self.assertTrue(rep["ok"])
        self.assertFalse(rep["tree_before_cgroup_kill"]["ok"])
        self.assertTrue(rep["tree"]["ok"])

    def test_a_tree_still_left_after_the_backstop_fails_the_sweep(self):
        self.tree_ok[:] = [False, False]
        self.arm.use_slot(17)
        self.assertFalse(self.arm.sweep(17)["ok"])


CG2 = "/sys/fs/cgroup"


@unittest.skipUnless(sys.platform.startswith("linux") and os.geteuid() == 0 and os.path.exists(os.path.join(CG2, "cgroup.subtree_control")),
                     "needs root on Linux with cgroup v2")
class LiveReplacedSlotTests(unittest.TestCase):
    """The same race with a real cgroup v2 and a real process: an old arm process's sweep of a replaced slot must not cgroup.kill the
    new run's process in the reused pool cgroup, and the slot's own process still empties it."""

    def setUp(self):
        self.d = tmp(self)
        make_slot(self.d, 17, 1.0)
        self.pool = os.path.join(CG2, f"drums-test-pool-{os.getpid()}")
        os.mkdir(self.pool)
        self.addCleanup(self._rm_pool)
        p = mock.patch.object(ps, "POOL", self.pool)
        p.start(), self.addCleanup(p.stop)
        q = mock.patch.object(ps.rt, "sweep_marker", lambda host, marker, *a, **kw: {"marker": marker, "ok": True, "failures": []})
        q.start(), self.addCleanup(q.stop)
        self.cg = ps.pool_cg("rootfs-x-17")
        os.mkdir(self.cg)

    def _rm_pool(self):
        for d in (os.path.join(self.pool, n) for n in os.listdir(self.pool)):
            if os.path.isdir(d):
                ps._write(os.path.join(d, "cgroup.kill"), "1")
                for _ in range(100):
                    if not ps._read(os.path.join(d, "cgroup.procs")):
                        break
                    time.sleep(0.05)
                os.rmdir(d)
        os.rmdir(self.pool)

    def arm(self):
        a = ps.PageshareArm.__new__(ps.PageshareArm)
        a.cfg = {"state_dir": self.d}
        a.ph = "proxy-host"
        a._ps = lambda slot: {"mode": "template", "socket": "/x.sock"}
        a._marker = lambda slot: f"rootfs-x-{slot}"
        a._argv_marker = lambda slot: "rootfs-x-0"
        return a

    def new_client(self):
        """A stand-in for the new run's pageshare client: a process that joins the slot's pool cgroup, as the rewritten restore argv does."""
        p = subprocess.Popen(["sh", "-c", 'echo $$ > "$0/cgroup.procs" && exec sleep 60', self.cg])
        self.addCleanup(lambda: (p.kill(), p.wait()))
        for _ in range(100):
            if str(p.pid) in ps._read(os.path.join(self.cg, "cgroup.procs")).split():
                return p
            time.sleep(0.02)
        self.fail("the stand-in client did not join the pool cgroup")

    def test_an_old_process_does_not_kill_the_new_run_in_the_reused_pool_cgroup(self):
        old = self.arm()
        old.use_slot(17)
        replace_slot(self.d, 17, 2.0)
        client = self.new_client()
        rep = old.sweep(17)
        time.sleep(0.3)
        self.assertIsNone(client.poll(), f"the new run's process was killed (rc={client.returncode}); sweep report {rep}")
        self.assertFalse(rep["ok"])

    def test_the_slots_own_process_still_empties_its_pool_cgroup(self):
        cur = self.arm()
        cur.use_slot(17)
        client = self.new_client()
        rep = cur.sweep(17)
        self.assertTrue(rep["ok"])
        self.assertEqual(client.wait(5), -9)


if __name__ == "__main__":
    unittest.main()
