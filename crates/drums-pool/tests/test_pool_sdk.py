#!/usr/bin/env python3
"""End-to-end tests of drums-pool with the unchanged drums SDK, against fake drumsd hosts.

    cargo build -p drums-pool && python3 crates/drums-pool/tests/test_pool_sdk.py

Each test starts its own fake hosts and pool on free ports and tears them down.
"""
import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import urllib.request

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.abspath(os.path.join(HERE, "..", "..", ".."))
sys.path.insert(0, os.path.join(ROOT, "python"))
import drums  # noqa: E402  (the SDK as released, unchanged)

POOL_BIN = os.environ.get("DRUMS_POOL_BIN", os.path.join(ROOT, "target", "debug", "drums-pool"))
HOST_TOKEN = "host-token-0123456789abcdef"
POOL_TOKEN = "pool-token-0123456789abcdef"


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    p = s.getsockname()[1]
    s.close()
    return p


def wait_http(url, timeout=10):
    t = time.time()
    while time.time() - t < timeout:
        try:
            urllib.request.urlopen(url, timeout=1).read()
            return
        except Exception:
            time.sleep(0.05)
    raise RuntimeError(f"{url} did not come up")


class Cluster:
    def __init__(self, n_hosts=3, max_slots=4, pool_cfg=None, host_args=None):
        self.tmp = tempfile.mkdtemp()
        self.hosts = []
        self.procs = []
        specs = []
        for i in range(n_hosts):
            port = free_port()
            args = [sys.executable, os.path.join(HERE, "fake_drumsd.py"), "--port", str(port), "--token", HOST_TOKEN, "--max-slots", str(max_slots)]
            args += (host_args or {}).get(i, [])
            p = subprocess.Popen(args)
            self.procs.append(p)
            self.hosts.append((f"h{i}", port, p))
            wait_http(f"http://127.0.0.1:{port}/healthz")
            specs.append({"name": f"h{i}", "url": f"http://127.0.0.1:{port}", "token": HOST_TOKEN, "region": "r1" if i % 2 == 0 else "r2", "cpus": 16, "max_envs": 100})
        tok = os.path.join(self.tmp, "token")
        open(tok, "w").write(POOL_TOKEN)
        self.port = free_port()
        cfg = {"listen": f"127.0.0.1:{self.port}", "token_file": tok, "hosts": specs, "health_interval_s": 0.2, "health_timeout_s": 0.5,
               "suspect_after": 2, "dead_after_s": 1.0, "reap_interval_s": 0.2, "young_env_s": 0.0, "refuse_penalty_s": 0.3,
               "event_log": os.path.join(self.tmp, "events.jsonl")}
        cfg.update(pool_cfg or {})
        cp = os.path.join(self.tmp, "pool.json")
        json.dump(cfg, open(cp, "w"))
        self.pool = subprocess.Popen([POOL_BIN, "--config", cp], stderr=open(os.path.join(self.tmp, "pool.log"), "w"))
        self.procs.append(self.pool)
        wait_http(f"http://127.0.0.1:{self.port}/healthz")
        self.url = f"http://127.0.0.1:{self.port}"
        # wait for every host to be healthy
        t = time.time()
        while time.time() - t < 10:
            h = self.admin("GET", "/pool/v1/hosts")
            if all(x["state"] == "healthy" for x in h["hosts"]):
                break
            time.sleep(0.1)

    def admin(self, m, path, body=None):
        req = urllib.request.Request(self.url + path, method=m, data=None if body is None else json.dumps(body).encode(),
                                     headers={"Authorization": f"Bearer {POOL_TOKEN}", "Content-Type": "application/json"})
        return json.loads(urllib.request.urlopen(req, timeout=30).read())

    def metrics(self):
        req = urllib.request.Request(self.url + "/metrics", headers={"Authorization": f"Bearer {POOL_TOKEN}"})
        return urllib.request.urlopen(req, timeout=10).read().decode()

    def events(self):
        return [json.loads(x) for x in open(os.path.join(self.tmp, "events.jsonl"))]

    def client(self):
        return drums.connect(self.url, POOL_TOKEN, close_on_exit=False, handle_signals=False)

    def kill_host(self, i):
        self.hosts[i][2].send_signal(signal.SIGSTOP)   # a hung host: TCP accepts nothing useful, polls time out

    def close(self):
        for p in self.procs:
            try:
                p.send_signal(signal.SIGCONT)
                p.kill()
            except Exception:
                pass


class PoolSDK(unittest.TestCase):
    def setUp(self):
        self.c = None

    def tearDown(self):
        if self.c:
            self.c.close()

    def test_sdk_roundtrip_ids_and_health(self):
        self.c = Cluster(3, 4)
        cl = self.c.client()
        h = cl.health()
        self.assertTrue(h["ok"])
        self.assertEqual(h["criu"]["sha256"], "a" * 64)    # the gateway's fingerprint fields pass through
        self.assertEqual(h["version"], "0.4.2")
        e = cl.create("osworld-ubuntu", task="task-0001")
        self.assertTrue(e.id.startswith("pe-"))
        self.assertNotIn("url", e.record)
        self.assertIn(e.record["pool"]["host"], {"h0", "h1", "h2"})
        e.step("print('ORACLE')")
        r = e.verify()
        self.assertTrue(r.valid)
        self.assertEqual(r.score, 1.0)
        e.reset()
        e.step("pass")
        self.assertEqual(e.verify().score, 0.0)
        self.assertEqual(len(e.screenshot()), 1008)
        self.assertEqual([x.id for x in cl.envs()], [e.id])
        e.close()
        with self.assertRaises(drums.EnvGone):
            e.refresh()

    def test_spread_and_backpressure_instead_of_refusals(self):
        # 3 hosts x 4 slots = 12; 20 concurrent creates: 12 run at once, 8 wait in the queue, none is refused
        self.c = Cluster(3, 4, host_args={i: ["--create-s", "0.3"] for i in range(3)})
        cl = self.c.client()
        out = []
        lock = threading.Lock()

        def worker(i):
            e = cl.create("osworld-ubuntu")
            time.sleep(0.5)
            with lock:
                out.append(e.record["pool"]["host"])
            e.close()
        th = [threading.Thread(target=worker, args=(i,)) for i in range(20)]
        [t.start() for t in th]
        [t.join() for t in th]
        self.assertEqual(len(out), 20)
        self.assertEqual(set(out), {"h0", "h1", "h2"})
        m = self.c.metrics()
        self.assertIn('pool_create_total{host="h0",kind="cold",outcome="ok"}', m)
        self.assertNotIn('outcome="error"', m)

    def test_host_refusal_is_absorbed(self):
        # the pool thinks each host has 100 slots; fakes refuse past 2 with 429: clients still get envs (retry elsewhere / wait)
        self.c = Cluster(2, 2)
        cl = self.c.client()
        envs = [cl.create("osworld-ubuntu") for _ in range(4)]
        res = []
        t = threading.Thread(target=lambda: res.append(cl.create("osworld-ubuntu")))
        t.start()
        time.sleep(1.0)
        envs[0].close()
        t.join(20)
        self.assertEqual(len(res), 1)
        self.assertIn("pool_host_refused_total", self.c.metrics())

    def test_queue_timeout_header(self):
        self.c = Cluster(1, 1, pool_cfg={"hosts": None} if False else None)
        cl = self.c.client()
        cl.create("osworld-ubuntu")
        req = urllib.request.Request(self.c.url + "/v1/slots", method="POST", data=json.dumps({"checkpoint": "osworld-ubuntu"}).encode(),
                                     headers={"Authorization": f"Bearer {POOL_TOKEN}", "Content-Type": "application/json", "X-Drums-Queue-Timeout": "1"})
        t = time.time()
        with self.assertRaises(urllib.error.HTTPError) as cm:
            urllib.request.urlopen(req, timeout=30)
        self.assertEqual(cm.exception.code, 503)
        self.assertEqual(json.loads(cm.exception.read())["code"], "queue_timeout")
        self.assertLess(time.time() - t, 5)

    def test_failover_replaces_envs_and_reset_continues(self):
        self.c = Cluster(3, 10, pool_cfg={"dead_after_s": 1.0})
        cl = self.c.client()
        envs = [cl.create("osworld-ubuntu", task="task-0001") for _ in range(9)]
        victims = [e for e in envs if e.record["pool"]["host"] == "h1"]
        self.assertTrue(victims)
        self.c.kill_host(1)
        t0 = time.time()
        # a step on a victim fails fast (not after a long timeout): Conflict episode_lost
        while True:
            try:
                victims[0].step("pass")
            except drums.Conflict as ex:
                self.assertIn("episode_lost", json.dumps(ex.body) if hasattr(ex, "body") else str(ex))
                break
            except drums.DrumsError:
                pass
            self.assertLess(time.time() - t0, 15)
            time.sleep(0.1)
        self.assertLess(time.time() - t0, 10)
        # the replacement is ready: reset continues on another host, and verify works
        victims[0].reset()
        self.assertNotEqual(victims[0].record["pool"]["host"], "h1")
        self.assertIn("pool_failover", victims[0].record)
        victims[0].step("print('ORACLE')")
        self.assertEqual(victims[0].verify().score, 1.0)
        # until a reset, the other victims refuse every op except get/reset/close: no reward from a lost episode
        for v in victims[1:]:
            with self.assertRaises(drums.Conflict):
                v.verify(strict=False)
            v.reset()
        # untouched envs on healthy hosts never noticed
        for e in envs:
            if e not in victims:
                e.step("pass")
        ev = [x["ev"] for x in self.c.events()]
        self.assertIn("host_dead", ev)
        self.assertIn("failover_done", ev)
        m = self.c.metrics()
        self.assertIn('pool_failover_total{outcome="replaced"} %d' % len(victims), m)

    def test_dead_host_rejoins_after_orphan_sweep(self):
        self.c = Cluster(2, 10, pool_cfg={"dead_after_s": 0.8})
        cl = self.c.client()
        envs = [cl.create("osworld-ubuntu") for _ in range(6)]
        self.c.kill_host(0)
        time.sleep(2.5)
        self.c.hosts[0][2].send_signal(signal.SIGCONT)
        t = time.time()
        while time.time() - t < 10:
            if any(x["ev"] == "host_rejoined" for x in self.c.events()):
                break
            time.sleep(0.1)
        ev = [x for x in self.c.events() if x["ev"] == "host_rejoined"]
        self.assertTrue(ev)
        self.assertGreaterEqual(ev[0]["orphans_deleted"], 1)
        for e in envs:
            try:
                e.reset()
            except drums.DrumsError:
                pass

    def test_lease_reclaim(self):
        self.c = Cluster(1, 4)
        cl = self.c.client()
        e = cl.create("osworld-ubuntu", idle_timeout=0.6)
        keep = cl.create("osworld-ubuntu", idle_timeout=0)
        time.sleep(1.5)
        with self.assertRaises(drums.EnvGone):
            e.refresh()
        keep.refresh()
        self.assertIn('pool_lease_reclaimed_total{why="lease_idle"} 1', self.c.metrics())

    def test_warm_slots_serve_creates(self):
        self.c = Cluster(2, 10, pool_cfg={"warm_per_host": 2}, host_args={0: ["--create-s", "1.0"], 1: ["--create-s", "1.0"]})
        time.sleep(3.0)
        cl = self.c.client()
        t = time.time()
        e = cl.create("osworld-ubuntu", task="task-0001")
        self.assertLess(time.time() - t, 0.5)     # served from a warm slot, not a 1 s restore
        self.assertEqual(e.record["task"]["id"], "task-0001")
        self.assertIn('kind="warm",outcome="ok"', self.c.metrics())

    def test_fork_ids_are_pool_ids(self):
        self.c = Cluster(2, 10)
        cl = self.c.client()
        e = cl.create("osworld-ubuntu")
        f = e.fork(3)
        kids = f.children if hasattr(f, "children") else list(f)
        self.assertEqual(len(kids), 3)
        for k in kids:
            self.assertTrue(k.id.startswith("pe-"))
            self.assertEqual(k.record["pool"]["host"], e.record["pool"]["host"])
            k.step("print('ORACLE')")
            self.assertEqual(k.verify().score, 1.0)
        st = f.state if hasattr(f, "state") else None
        self.assertTrue(str(st).startswith("ps-"), st)
        k2 = cl.create(from_state=st)
        self.assertEqual(k2.record["pool"]["host"], e.record["pool"]["host"])
        self.assertEqual(k2.record["checkpoint"], str(st))

    def test_auth(self):
        self.c = Cluster(1, 2)
        with self.assertRaises(drums.AuthError):
            drums.connect(self.c.url, "wrong-token-xxxxxxxxxxxx", close_on_exit=False, handle_signals=False)


if __name__ == "__main__":
    unittest.main(verbosity=2)
