"""tmplsrv.py (template server protection, death detection and restart) and its use in pageshare.py, with fakes.
These prove the harness logic only; the live memory-pressure runs are the evidence (runbooks/oom-inotify)."""

import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from unittest import mock

sys.path.insert(0, os.path.dirname(__file__))

from sharedos_rootfs import pageshare as ps  # noqa: E402
from sharedos_rootfs import tmplsrv  # noqa: E402
from sharedos_rootfs.arm import ArmError  # noqa: E402

CLIENT = os.path.join(os.path.dirname(os.path.dirname(os.path.abspath(__file__))), "sharedos_rootfs", "pageshare_client.py")


def tmp(tc):
    d = tempfile.mkdtemp()
    tc.addCleanup(shutil.rmtree, d, True)
    return d


class RequireTests(unittest.TestCase):
    """require(): alive -> nothing; dead -> one restart from the spec, recorded; no spec or a failed restart -> raise."""

    def setUp(self):
        self.d = tmp(self)
        self.sock = os.path.join(self.d, "ps-0.sock")
        self.spec = {"socket": self.sock, "images": "/s/slot0/images", "criu": "/c/criu", "pidfile": os.path.join(self.d, "ps-0.pid"),
                     "log": os.path.join(self.d, "ps-0-server-1.log"), "out": os.path.join(self.d, "ps-0-server.out"),
                     "cgroup": "/cg/x", "generation": 1}

    def write_spec(self):
        with open(tmplsrv.spec_path(self.sock), "w") as f:
            json.dump(self.spec, f)

    def events(self):
        p = tmplsrv.events_path(self.sock)
        return [json.loads(l) for l in open(p)] if os.path.exists(p) else []

    def test_an_alive_server_is_not_touched(self):
        with mock.patch.object(tmplsrv, "status", return_value={"alive": True, "pid": 5, "generation": 1}), \
                mock.patch.object(tmplsrv, "start") as st:
            r = tmplsrv.require(self.sock)
        self.assertFalse(r["restarted"])
        st.assert_not_called()
        self.assertEqual(self.events(), [])

    def test_a_dead_server_is_restarted_from_its_spec_and_the_event_is_recorded(self):
        self.write_spec()
        dead = {"alive": False, "pid": 5, "why": "pid 5 is gone", "cgroup_oom_kills": 1}
        with mock.patch.object(tmplsrv, "status", return_value=dead), \
                mock.patch.object(tmplsrv, "start", return_value={"pid": 9, "generation": 2, "memory_current": 123}) as st:
            r = tmplsrv.require(self.sock)
        self.assertTrue(r["restarted"])
        self.assertEqual(r["pid"], 9)
        args = st.call_args[0]
        self.assertEqual(args[:3], ("/c/criu", "/s/slot0/images", self.sock))
        self.assertEqual(args[3], os.path.join(self.d, "ps-0-server-1-g2.log"))      # the dead server's log is kept
        self.assertEqual(args[6], 2)
        ev = self.events()
        self.assertEqual(len(ev), 1)
        self.assertEqual(ev[0]["action"], "restarted")
        self.assertEqual(ev[0]["status"]["cgroup_oom_kills"], 1)

    def test_a_dead_server_without_a_spec_fails_loudly_and_is_recorded(self):
        with mock.patch.object(tmplsrv, "status", return_value={"alive": False, "why": "no readable pid file"}):
            with self.assertRaises(ArmError) as c:
                tmplsrv.require(self.sock)
        self.assertIn("is dead", str(c.exception))
        self.assertEqual(self.events()[0]["action"], "none (no spec)")

    def test_a_failed_restart_raises_with_the_evidence(self):
        self.write_spec()
        with mock.patch.object(tmplsrv, "status", return_value={"alive": False, "why": "pid 5 is gone", "cgroup_oom_kills": 2}), \
                mock.patch.object(tmplsrv, "start", side_effect=ArmError("template server did not open the socket in 120 s")):
            with self.assertRaises(ArmError) as c:
                tmplsrv.require(self.sock)
        self.assertIn("restart failed", str(c.exception))
        self.assertIn("oom_kill=2", str(c.exception))
        self.assertEqual(self.events()[0]["action"], "restart failed")

    def test_a_restart_done_by_another_slot_while_waiting_is_reused(self):
        self.write_spec()
        seq = iter([{"alive": False, "why": "gone"}, {"alive": True, "pid": 9, "generation": 2}])
        with mock.patch.object(tmplsrv, "status", side_effect=lambda s: next(seq)), mock.patch.object(tmplsrv, "start") as st:
            r = tmplsrv.require(self.sock)
        st.assert_not_called()
        self.assertTrue(r.get("waited_for_restart"))


class StatusTests(unittest.TestCase):
    def test_a_pid_that_is_not_the_server_is_dead(self):
        d = tmp(self)
        sock = os.path.join(d, "ps-0.sock")
        open(sock, "w").close()
        with open(os.path.join(d, "ps-0.pid"), "w") as f:
            f.write(str(os.getpid()))                 # alive, but this test process is no template server
        st = tmplsrv.status(sock)
        self.assertFalse(st["alive"])
        self.assertIn("is not the template server", st["why"])

    def test_a_gone_pid_is_dead(self):
        d = tmp(self)
        sock = os.path.join(d, "ps-0.sock")
        p = subprocess.Popen(["true"])
        p.wait()
        with open(os.path.join(d, "ps-0.pid"), "w") as f:
            f.write(str(p.pid))
        st = tmplsrv.status(sock)
        self.assertFalse(st["alive"])
        self.assertIn("gone", st["why"])


class SlotOomGateTests(unittest.TestCase):
    """slot_oom_check reads the real /proc of processes listed in a fake cgroup."""

    def cg_with(self, pids):
        d = tmp(self)
        with open(os.path.join(d, "cgroup.procs"), "w") as f:
            f.write("\n".join(str(p) for p in pids))
        return d

    def child(self, adj):
        p = subprocess.Popen(["sh", "-c", f'echo {adj} > /proc/self/oom_score_adj && exec sleep 30'])
        self.addCleanup(p.kill)
        for _ in range(100):
            try:
                if int(open(f"/proc/{p.pid}/oom_score_adj").read()) == adj:
                    break
            except (OSError, ValueError):
                pass
            time.sleep(0.02)
        return p.pid

    @unittest.skipUnless(sys.platform.startswith("linux"), "needs /proc")
    def test_an_init_at_the_slot_value_passes(self):
        pid = self.child(tmplsrv.SLOT_OOM_SCORE_ADJ)
        rep = tmplsrv.slot_oom_check(self.cg_with([pid]), pid)
        self.assertEqual(rep["init_oom_score_adj"], tmplsrv.SLOT_OOM_SCORE_ADJ)

    @unittest.skipUnless(sys.platform.startswith("linux"), "needs /proc")
    def test_an_init_that_inherited_another_value_fails_closed(self):
        pid = self.child(500)
        with self.assertRaises(ArmError) as c:
            tmplsrv.slot_oom_check(self.cg_with([pid]), pid)
        self.assertIn("OOM priority gate failed", str(c.exception))


class ClientTests(unittest.TestCase):
    def test_a_stale_socket_fails_at_once_with_no_template_server(self):
        d = tmp(self)
        sock = os.path.join(d, "ps-0.sock")
        s = socket.socket(socket.AF_UNIX, socket.SOCK_SEQPACKET)
        s.bind(sock)
        s.close()                                     # the file stays, nobody listens: what a dead server leaves
        t0 = time.monotonic()
        r = subprocess.run([sys.executable, "-I", CLIENT, sock, "criu", "restore"], capture_output=True, text=True, timeout=60)
        self.assertEqual(r.returncode, 1)
        self.assertIn("no template server", r.stderr)
        self.assertLess(time.monotonic() - t0, 5)     # it used to wait 120 s

    def test_a_missing_socket_fails_at_once(self):
        r = subprocess.run([sys.executable, "-I", CLIENT, "/nonexistent/ps.sock", "criu", "restore"], capture_output=True, text=True,
                           timeout=60)
        self.assertIn("no template server", r.stderr)


class FakeArm:
    """PageshareArm methods on a fake slot.json in a temp dir."""

    def __init__(self, tc, mode="template"):
        self.d = tmp(tc)
        os.makedirs(os.path.join(self.d, "slot3"))
        arm = ps.PageshareArm.__new__(ps.PageshareArm)
        arm.cfg = {"state_dir": self.d, "vcpus": 4, "memory_mib": 4096}
        arm._marker = lambda slot: f"rootfs-ab-{slot}"
        arm._cur_slot = 3
        self.slot_json = os.path.join(self.d, "slot3", "slot.json")
        with open(self.slot_json, "w") as f:
            json.dump({"pageshare": {"mode": mode, "socket": os.path.join(self.d, "ps-0.sock"), "criu": None}}, f)

        def record(slot, **kw):
            st = json.load(open(self.slot_json))
            st.update(kw)
            with open(self.slot_json, "w") as f:
                json.dump(st, f)
            return st
        arm._record = record
        self.arm = arm

    def st(self):
        return json.load(open(self.slot_json))


class RewriteTests(unittest.TestCase):
    ARGV = ["systemd-run", "--scope", "-u", "x", "--", "unshare", "--mount", "--", "criu", "restore", "-D", "/i"]

    def test_the_restore_runs_at_the_slot_value_and_the_server_is_checked_first(self):
        f = FakeArm(self)
        with mock.patch.object(ps, "ensure_slot_cg", return_value="/cg/slot"), \
                mock.patch.object(tmplsrv, "require", return_value={"alive": True, "restarted": False}) as rq:
            out = f.arm._rewrite(list(self.ARGV), f.arm._ps(3))
        rq.assert_called_once()
        self.assertEqual(out[:5], ["sh", "-c", out[2], "/cg/slot", str(tmplsrv.SLOT_OOM_SCORE_ADJ)])
        self.assertIn("/proc/self/oom_score_adj", out[2])
        cg = tmp(self)
        r = subprocess.run(["sh", "-c", out[2], cg, "7", "echo", "ARGS"], capture_output=True, text=True)
        self.assertEqual(r.stdout, "ARGS\n", r.stderr)  # the value is consumed by the wrapper, not passed to the restore
        self.assertTrue(open(os.path.join(cg, "cgroup.procs")).read().strip().isdigit())

    def test_a_dead_server_that_cannot_be_restarted_fails_the_restore(self):
        f = FakeArm(self)
        with mock.patch.object(ps, "ensure_slot_cg", return_value="/cg/slot"), \
                mock.patch.object(tmplsrv, "require", side_effect=ArmError("template server for x is dead")):
            with self.assertRaises(ArmError):
                f.arm._rewrite(list(self.ARGV), f.arm._ps(3))

    def test_a_restart_is_recorded_in_the_slot(self):
        f = FakeArm(self)
        with mock.patch.object(ps, "ensure_slot_cg", return_value="/cg/slot"), \
                mock.patch.object(tmplsrv, "require", return_value={"alive": True, "restarted": True, "event": {"action": "restarted"}}):
            f.arm._rewrite(list(self.ARGV), f.arm._ps(3))
        self.assertEqual(f.st()["last_template_restart"], {"action": "restarted"})


class EpisodeOomTests(unittest.TestCase):
    """The reset's record names the OOM kills of the episode that ended; an OOM kill during the restore fails it."""

    def test_kills_during_the_episode_are_reported_once(self):
        f = FakeArm(self)
        counter = {"n": 0}
        with mock.patch.object(tmplsrv, "oom_kills", side_effect=lambda cg: counter["n"]), \
                mock.patch.object(tmplsrv, "slot_oom_check", return_value={"ok": 1}), \
                mock.patch.object(ps.BranchArm, "_verify_cgroup", lambda slf, slot, pid: {}):
            f.arm._oom_audit(3)                       # first restore: no episode yet
            f.arm._verify_cgroup(3, 100)              # restore done: episode opens at 0
            counter["n"] = 2                          # two tasks of the slot OOM-killed during the episode
            f.arm._oom_audit(3)                       # the reset's sweep
            self.assertEqual(f.st()["last_episode_oom_kills"], 2)
            f.arm._oom_audit(3)                       # a second sweep (failure path) does not report it again
            self.assertIsNone(f.st()["last_episode_oom_kills"])
            f.arm._verify_cgroup(3, 101)
            f.arm._oom_audit(3)
            self.assertEqual(f.st()["last_episode_oom_kills"], 0)

    def test_an_oom_kill_during_the_restore_fails_it(self):
        f = FakeArm(self)
        counter = {"n": 4}
        with mock.patch.object(tmplsrv, "oom_kills", side_effect=lambda cg: counter["n"]), \
                mock.patch.object(tmplsrv, "slot_oom_check", return_value={"ok": 1}), \
                mock.patch.object(ps.BranchArm, "_verify_cgroup", lambda slf, slot, pid: {}):
            f.arm._oom_audit(3)                       # the reset's sweep: baseline 4
            counter["n"] = 5
            with self.assertRaises(ArmError) as c:
                f.arm._verify_cgroup(3, 100)
        self.assertIn("during this restore", str(c.exception))


if __name__ == "__main__":
    unittest.main()
