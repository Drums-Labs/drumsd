"""drums_osw.py and the verifier sandbox (docs/design/verifier-sandbox.md): a task that names `verifier_sha256` is
scored through drumsd's sandbox URL, never loaded by the helper; every reply names the sha256 that scored it.
A stand-in drumsd answers the sandbox calls; the sandbox itself is tested in crates/drumsd/tests/verifier_sandbox.rs."""
import hashlib
import json
import os
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import test_osw_verifier as base  # noqa: E402

osw = base.osw
SHA = "ab" * 32
ALT = "cd" * 32


class FakeDrumsd(BaseHTTPRequestHandler):
    bodies = []
    verdicts = {}

    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("Content-Length") or 0)))
        FakeDrumsd.bodies.append(body)
        v = dict(FakeDrumsd.verdicts.get((body["sha256"], body["function"]),
                                         {"score": 1.0, "info": {"ok": True}, "error": None, "error_class": "none"}))
        v.update(sha256=body["sha256"], function=body["function"], seconds=0.1, cpu_s=0.05, memory_peak_mib=12.0)
        b = json.dumps(v).encode()
        self.send_response(200)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)


def hashed_task(function="check", sha=SHA, **options):
    return {"id": "acme-test-02", "instruction": "x", "config": [],
            "evaluator": {"func": "drums_verifier", "verifier_sha256": sha, "function": function, "options": options}}


class Sandboxed(base.LabVerifier):
    @classmethod
    def setUpClass(cls):
        super().setUpClass()
        cls.drumsd = ThreadingHTTPServer(("127.0.0.1", 0), FakeDrumsd)
        threading.Thread(target=cls.drumsd.serve_forever, daemon=True).start()

    @classmethod
    def tearDownClass(cls):
        cls.drumsd.shutdown()
        super().tearDownClass()

    def setUp(self):
        FakeDrumsd.bodies, FakeDrumsd.verdicts = [], {}

    def req(self, task, vdir=None, named=False):
        r = super().req(task, vdir)
        r["sandbox_url"] = f"http://127.0.0.1:{self.drumsd.server_address[1]}/provision/tok/verifier"
        r["sandbox_named"] = named
        return r

    def test_a_hashed_verifier_is_scored_by_the_sandbox_with_its_hash_in_the_reply(self):
        r = osw.run_request("evaluate", self.req(hashed_task(want=3)))
        self.assertEqual((r["score"], r["checker_error"], r["info"]), (1.0, None, {"ok": True}), r)
        self.assertEqual(r["verifier_sha256"], SHA)
        self.assertEqual(r["sandbox"]["sha256"], SHA)
        self.assertEqual(r["sandbox"]["cpu_s"], 0.05)
        first = FakeDrumsd.bodies[0]
        self.assertEqual((first["sha256"], first["function"], first["options"]), (SHA, "check", {"want": 3}))
        self.assertEqual(first["deadline_s"], osw.DEFAULT_CHECKER_DEADLINE_S)
        self.assertEqual(first["task"]["id"], "acme-test-02")
        self.assertEqual(len(FakeDrumsd.bodies), 2, "the recheck is a second sandbox call")
        self.assertEqual(r["audit"]["flags"], [])

    def test_sandbox_error_classes_keep_their_audit_meaning(self):
        for cls, needle, err_class, flag in (("timeout", "CheckerTimeout", "timeout", "checker_timeout"),
                                             ("bad_return", "finite number", "bad_return", "checker_crash"),
                                             ("oom", "oom: too big", "exception", "checker_crash"),
                                             ("exception", "ValueError", "exception", "checker_crash")):
            FakeDrumsd.verdicts = {(SHA, "check"): {"score": None, "info": None, "error": "too big" if cls == "oom" else "ValueError: x",
                                                    "error_class": cls}}
            r = osw.run_request("evaluate", self.req(hashed_task()))
            self.assertIsNone(r["score"], cls)
            self.assertIn(needle, r["checker_error"], cls)
            self.assertEqual((r["audit"]["checker"]["error_class"], r["audit"]["flags"]), (err_class, [flag]), cls)
            self.assertEqual(r["verifier_sha256"], SHA)

    def test_the_redundant_checker_defaults_to_the_primarys_hash_or_names_its_own(self):
        t = hashed_task()
        t["evaluator"]["redundant"] = {"function": "check_alt"}
        a = osw.run_request("evaluate", self.req(t))["audit"]
        self.assertEqual((a["redundant"]["verifier_sha256"], a["redundant"]["agree"]), (SHA, True))
        t["evaluator"]["redundant"] = {"function": "check_alt", "verifier_sha256": ALT}
        FakeDrumsd.verdicts = {(ALT, "check_alt"): {"score": 0.0, "info": None, "error": None, "error_class": "none"}}
        r = osw.run_request("evaluate", self.req(t))
        self.assertEqual(r["audit"]["redundant"]["verifier_sha256"], ALT)
        self.assertIn("redundant_disagree", r["audit"]["flags"])

    def test_without_a_sandbox_url_a_hashed_verifier_is_a_checker_error(self):
        req = base.LabVerifier.req(self, hashed_task())
        r = osw.run_request("evaluate", req)
        self.assertIsNone(r["score"])
        self.assertIn("sandbox_url", r["checker_error"])

    def test_a_named_verifier_reports_the_hash_of_the_file_that_ran(self):
        r = osw.run_request("evaluate", self.req(base.lab_task("boolean")))
        want = hashlib.sha256(open(os.path.join(self.vdir, "lab.py"), "rb").read()).hexdigest()
        self.assertEqual((r["score"], r["verifier_sha256"]), (1.0, want))
        self.assertNotIn("sandbox", r)
        self.assertEqual(FakeDrumsd.bodies, [], "sandbox_named off: the named file runs here, as before")

    def test_sandbox_named_sends_a_named_verifier_to_the_sandbox_by_its_hash(self):
        want = hashlib.sha256(open(os.path.join(self.vdir, "lab.py"), "rb").read()).hexdigest()
        r = osw.run_request("evaluate", self.req(base.lab_task("boolean"), named=True))
        self.assertEqual((r["score"], r["verifier_sha256"]), (1.0, want))
        self.assertEqual((FakeDrumsd.bodies[0]["sha256"], FakeDrumsd.bodies[0]["named"]), (want, "lab"))


# discovery also collects the inherited LabVerifier tests here: they run with a sandbox URL set and must still pass

if __name__ == "__main__":
    unittest.main()
