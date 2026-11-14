"""lockprobe: the kernel phase runs for real (no CRIU, no fakes): it shows what THIS kernel does to a POSIX lock when a second fd of the file is mapped
and closed. The CRIU phase is exercised with a fake runner only; nothing here says what CRIU does on the operator's host."""

import json
import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))

from sharedos_rootfs import lockprobe as lp  # noqa: E402
from sharedos_rootfs import locks as lk  # noqa: E402


def tmp(tc):
    d = tempfile.mkdtemp()
    tc.addCleanup(shutil.rmtree, d, True)
    return d


class KernelPhaseTests(unittest.TestCase):
    def test_a_posix_lock_is_released_by_closing_a_second_fd_and_a_flock_is_not(self):
        r = lp.kernel_phase(tmp(self))
        self.assertTrue(r["posix"]["before"], "the POSIX lock must be visible in /proc/locks before the close")
        self.assertFalse(r["posix"]["lock_survived"])
        self.assertTrue(r["flock"]["lock_survived"])
        self.assertIn("POSIX lock released", r["conclusion"])

    def test_cli_refuses_an_existing_evidence_dir_and_writes_the_kernel_result(self):
        import contextlib
        import io
        d = tmp(self)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(lp.main(["--evidence", d]), 2)
        ev = os.path.join(d, "ev")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(lp.main(["--evidence", ev]), 0)
        self.assertIn("kernel", json.load(open(os.path.join(ev, "lockprobe-result.json"))))


class FakeChild:
    pid = 4242

    def __init__(self, argv, **kw):
        self.d = argv[-1]
        open(os.path.join(self.d, "ready"), "w").close()

    def wait(self, timeout=None):
        return -9


class CriuPhaseTests(unittest.TestCase):
    def probe(self, pre_resume, dumped=2, rc=(0, 0)):
        ev = tmp(self)
        owned_kills = []

        def run(argv, timeout=300):
            d = os.path.join(ev, "criu-lock")
            if argv[1] == "dump":
                with open(argv[argv.index("-o") + 1], "w") as f:
                    f.write("".join(f"LOCK flag: 1,type: 0,pid: 52,fd: {fd},start: {s:8x},len: {n:8x}\n" for fd, s, n in [(8, 0x40000002, 0x1fe), (10, 0x80, 1)][:dumped]))
                return rc[0], "", "boom"
            with open(argv[argv.index("-o") + 1], "w") as f:
                f.write("(posix)flag: 1, type: 0, pid: 52, fd: 8, start: 40000002, len:      1fe\n(posix)flag: 1, type: 0, pid: 52, fd: 10, start:       80, len:        1\n")
            open(argv[argv.index("--pidfile") + 1], "w").write("5151")
            with open(os.path.join(d, "hook", "locks-pre-resume.json"), "w") as f:
                json.dump(pre_resume, f)
            return rc[1], "", "boom"
        p = lp.Probe(ev, run=run, popen=FakeChild, sleep=lambda s: None, start_ticks=lambda pid: "77")
        p._census = lambda pid, files: {"fdinfo": [], "host_proc_locks": {}}
        p._stop = lambda pid: owned_kills.append(pid)
        return p, ev, owned_kills

    def locks(self, *paths):
        return {"locks": [{"path": x} for x in paths], "host_proc_locks_of_tree": ["1: POSIX  ADVISORY  READ 5151 00:2e:9 1073741826 1073742335"]}

    def test_mapped_file_lock_lost_while_the_plain_one_survives_confirms_the_mechanism(self):
        p, ev, kills = self.probe(self.locks("/x/A.db"))
        r = p.criu_phase()
        self.assertTrue(r["verdict"].startswith("CONFIRMED"))
        self.assertIn("host's /proc/locks", r["verdict"])
        self.assertEqual(sorted(kills), [5151])                       # the dumped original was reaped (not signalled); the restored tree is stopped
        self.assertEqual(len(r["applied_log_locks"]), 2)

    def test_both_locks_present_refutes_it(self):
        p, ev, kills = self.probe(self.locks("/x/A.db", "/x/B.db-shm"))
        self.assertTrue(p.criu_phase()["verdict"].startswith("NOT REPRODUCED"))

    def test_nothing_held_is_unexpected_and_a_missing_or_failed_census_is_inconclusive(self):
        p, ev, kills = self.probe(self.locks())
        self.assertTrue(p.criu_phase()["verdict"].startswith("UNEXPECTED"))
        p, ev, kills = self.probe({"error": "no CRTOOLS_INIT_PID"})
        self.assertTrue(p.criu_phase()["verdict"].startswith("inconclusive"))
        p, ev, kills = self.probe(self.locks("/x/A.db"), dumped=1)
        self.assertIn("expected 2", p.criu_phase()["verdict"])

    def test_dump_or_restore_failure_is_reported_and_the_child_is_still_stopped(self):
        for rc in ((1, 0), (0, 1)):
            p, ev, kills = self.probe(self.locks("/x/A.db"), rc=rc)
            with self.assertRaisesRegex(RuntimeError, "criu (dump|restore) failed"):
                p.criu_phase()
            if rc[0]:
                self.assertIn(4242, kills)                                # a failed dump leaves the child alive: still owned and stopped
            else:
                self.assertNotIn(4242, kills)                             # a dumped child is already dead and reaped: its pid is not signalled

    def test_only_the_probes_own_identity_is_ever_signalled(self):
        ev = tmp(self)
        p = lp.Probe(ev, start_ticks=lambda pid: "later-process")
        p.owned[4242] = "original"
        self.assertIn("not signalled", p._stop(4242))
        self.assertIn("not signalled", p._stop(999999))


class AppliedLogTests(unittest.TestCase):
    def test_the_restorers_deferred_posix_lines_parse_with_their_0x_values(self):
        log = ("(00.07) pie: 24068: (posix)flag: 1, type: 0, pid: 24068, fd: 3, start: 0x40000002, len: 0x1fe (set after VMA fd close)\n"
               "(00.07) pie: 24068: (posix)flag: 1, type: 0, pid: 24068, fd: 4, start: 0x80, len: 0x1 (set after VMA fd close)\n"
               "(00.07) (flock)flag: 2, type: 0, cmd: 1, pid: 5, fd: 6\n")
        self.assertEqual(lk.applied_from_log(log), sorted([[1, 0, 24068, 3, 1073741826, 510], [1, 0, 24068, 4, 128, 1], [2, 0, 5, 6, None, None]], key=str))


class ReapTests(unittest.TestCase):
    """Real subprocesses: a killed child stays a zombie holding its pid until its parent waits, which is what made the restore fail with 'File exists'."""

    def test_a_killed_child_is_a_zombie_until_reaped_and_the_probe_reaps_it(self):
        import signal
        import subprocess
        c = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        self.addCleanup(lambda: c.poll() is None and c.kill())
        os.kill(c.pid, signal.SIGKILL)                                    # what `criu dump` does to the dumped task
        for _ in range(100):
            st = lp._ticks(c.pid)
            with open(f"/proc/{c.pid}/stat") as f:
                state = f.read().rsplit(")", 1)[1].split()[0]
            if state == "Z":
                break
            import time
            time.sleep(0.02)
        self.assertEqual(state, "Z")
        p = lp.Probe(tmp(self))
        p.owned[c.pid] = st
        self.assertEqual(p._reap(c), -9)
        self.assertFalse(os.path.exists(f"/proc/{c.pid}"))                # the pid is free for the restore
        self.assertNotIn(c.pid, p.owned)

    def test_an_unreaped_live_child_stops_the_probe_before_any_restore(self):
        import subprocess
        c = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
        self.addCleanup(lambda: (c.kill(), c.wait()))
        p = lp.Probe(tmp(self))
        with self.assertRaisesRegex(RuntimeError, "not reaped"):
            p._reap(c, timeout=0.2)

    def test_the_restore_is_only_attempted_after_the_reap(self):
        order = []

        class Child(FakeChild):
            def wait(self, timeout=None):
                order.append("reap")
                return -9
        ev = tmp(self)

        def run(argv, timeout=300):
            order.append(argv[1])
            if argv[1] == "dump":
                open(argv[argv.index("-o") + 1], "w").write("")
                return 0, "", ""
            return 1, "", "stop here"
        p = lp.Probe(ev, run=run, popen=Child, sleep=lambda s: None, start_ticks=lambda pid: "77", cmdline=lambda pid: None)
        p._census = lambda pid, files: {"fdinfo": [], "host_proc_locks": {}}
        with self.assertRaises(RuntimeError):
            p.criu_phase()
        self.assertEqual(order, ["dump", "reap", "restore"])


class CleanupGapTests(unittest.TestCase):
    """The tiny CRIU probe is run as root by the operator, so every process it might signal must be provably its own."""

    def make(self, restore=None, popen=None, ticks=None, cmds=None, wait_s=30.0, dump_rc=0):
        ev = tmp(self)
        kills = []
        ticks = ticks if ticks is not None else {4242: "77"}
        cmds = cmds if cmds is not None else {}
        child_argv = [sys.executable, "-c", lp.CHILD, os.path.join(ev, "criu-lock", "files")]

        def run(argv, timeout=300):
            if argv[1] == "dump":
                open(argv[argv.index("-o") + 1], "w").write("LOCK flag: 1,type: 0,pid: 52,fd: 8,start: 40000002,len:      1fe\nLOCK flag: 1,type: 0,pid: 52,fd: 10,start:       80,len:        1\n")
                return dump_rc, "", "boom"
            return restore(argv, timeout)
        p = lp.Probe(ev, run=run, popen=popen or FakeChild, sleep=lambda s: None, start_ticks=lambda pid: ticks.get(pid), cmdline=lambda pid: cmds.get(pid),
                     kill=lambda pid, sig: kills.append((pid, sig)), wait_s=wait_s)
        p._census = lambda pid, files: {"fdinfo": [], "host_proc_locks": {}}
        return p, ev, kills, ticks, cmds, child_argv

    def test_the_child_is_owned_before_the_readiness_wait_and_stopped_when_it_times_out(self):
        class Silent:
            pid = 4242

            def __init__(self, argv, **kw):
                pass                                                      # never writes the ready file
        t = {"n": 0}
        p, ev, kills, ticks, cmds, argv = self.make(restore=None, popen=Silent, wait_s=1.0)
        p.clock = lambda: t.__setitem__("n", t["n"] + 0.5) or t["n"]
        with self.assertRaisesRegex(RuntimeError, "never became ready"):
            p.criu_phase()
        self.assertEqual(kills, [(4242, 9)])                              # owned immediately, so the timeout path still stops it

    def test_a_partially_restored_tree_is_adopted_and_stopped_when_restore_fails(self):
        def restore(argv, timeout):
            ticks[4242] = "88"                                            # CRIU restored a NEW incarnation under the original pid, then failed
            cmds[4242] = argv_child
            open(argv[argv.index("-o") + 1], "w").write("(00.9) Error (criu/cr-restore.c): restore failed\n")
            return 1, "", "boom"
        p, ev, kills, ticks, cmds, argv_child = self.make(restore=restore)
        with self.assertRaisesRegex(RuntimeError, "criu restore failed"):
            p.criu_phase()
        res = json.load(open(os.path.join(ev, "criu-lock", "criu-phase-result.json")))
        self.assertEqual(res["partial_restore"]["state"], "partially restored incarnation adopted (it will be stopped)")
        self.assertIn("Error (criu/cr-restore.c)", res["restore_log_tail"])
        self.assertIn([4242, 9], [list(k) for k in kills])
        self.assertEqual(p.owned[4242], "88")                             # the identity that was signalled is the NEW incarnation's

    def test_a_timed_out_restore_is_captured_the_same_way(self):
        def restore(argv, timeout):
            ticks[4242] = "88"
            cmds[4242] = argv_child
            raise subprocess.TimeoutExpired(argv, timeout)
        import subprocess
        p, ev, kills, ticks, cmds, argv_child = self.make(restore=restore)
        with self.assertRaisesRegex(RuntimeError, "restore timed out"):
            p.criu_phase()
        res = json.load(open(os.path.join(ev, "criu-lock", "criu-phase-result.json")))
        self.assertTrue(res["partial_restore"]["cmdline_matches"])
        self.assertIsNone(res["restore_rc"])
        self.assertTrue(kills)

    def test_another_program_at_the_original_pid_is_never_adopted_or_signalled(self):
        def restore(argv, timeout):
            ticks[4242] = "999"                                           # pid reuse by something else
            cmds[4242] = ["/usr/sbin/sshd", "-D"]
            return 1, "", "boom"
        p, ev, kills, ticks, cmds, argv_child = self.make(restore=restore)
        with self.assertRaises(RuntimeError):
            p.criu_phase()
        res = json.load(open(os.path.join(ev, "criu-lock", "criu-phase-result.json")))
        self.assertIn("not adopted, not signalled", res["partial_restore"]["state"])
        for pid, sig in kills:
            self.assertNotEqual(ticks.get(pid), "999")                    # nothing with the foreign identity was signalled
        self.assertEqual(res["cleanup_problems"], [])                     # the reaped original is no longer owned, so its reused pid is never touched

    def test_no_process_at_the_pid_means_nothing_partial_and_no_stray_kill(self):
        def restore(argv, timeout):
            ticks.pop(4242, None)
            return 1, "", "boom"
        p, ev, kills, ticks, cmds, argv_child = self.make(restore=restore)
        with self.assertRaises(RuntimeError):
            p.criu_phase()
        res = json.load(open(os.path.join(ev, "criu-lock", "criu-phase-result.json")))
        self.assertIn("nothing partial", res["partial_restore"]["state"])
        self.assertEqual(kills, [])

    def test_the_still_running_original_after_a_failed_dump_is_stopped_by_identity(self):
        p, ev, kills, ticks, cmds, argv_child = self.make(dump_rc=1, restore=lambda a, t: (0, "", ""))
        with self.assertRaisesRegex(RuntimeError, "criu dump failed"):
            p.criu_phase()
        self.assertEqual(kills, [(4242, 9)])

    def test_the_result_records_the_source_hashes_and_the_two_file_evidence(self):
        h = lp.source_hashes()
        self.assertEqual(sorted(h), ["child_source", "lockprobe.py", "locks.py"])
        self.assertTrue(all(len(v) == 64 for v in h.values()))
        self.assertIn("A.db", lp.CHILD)
        self.assertIn("B.db-shm", lp.CHILD)                                # the plain file and the file that is also mmapped
        self.assertIn("mmap.mmap", lp.CHILD)


if __name__ == "__main__":
    unittest.main()
