#!/usr/bin/env python3
"""A fake drumsd for pool tests: the API surface the SDK and the pool use, with no desktops behind it.

    python3 fake_drumsd.py --port 7101 --token TOKEN [--max-slots 8] [--create-s 0.2] [--mem-total-mib 65536]
                          [--env-mib 1024] [--load 1.0]

Slots are records in memory. A create past --max-slots answers 429 overloaded (like drumsd admission control).
/v1/health reports memory that shrinks by --env-mib per live slot, so the pool's headroom math sees real numbers.
verify returns score 1.0 when the env's last step code contains "ORACLE", else 0.0; valid=True unless the env was
created from a checkpoint marked bad.
"""
import argparse
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

ap = argparse.ArgumentParser()
ap.add_argument("--port", type=int, required=True)
ap.add_argument("--token", required=True)
ap.add_argument("--max-slots", type=int, default=8)
ap.add_argument("--create-s", type=float, default=0.2)
ap.add_argument("--mem-total-mib", type=float, default=65536)
ap.add_argument("--env-mib", type=float, default=1024)
ap.add_argument("--load", type=float, default=1.0)
ap.add_argument("--criu-sha", default="a" * 64)
A = ap.parse_args()

lock = threading.Lock()
slots = {}
ckpts = {}
seq = [100]
stats = {"creates": 0, "refused": 0, "deletes": 0}
TASK = {"id": "task-0001", "config": [], "evaluator": {"func": "x"}, "instruction": "fake task"}


def live():
    return [s for s in slots.values() if s["state"] != "destroyed"]


def new_slot(ck, task):
    seq[0] += 1
    sid = f"s{seq[0]}"
    now = time.time()
    r = {"id": sid, "num": seq[0], "checkpoint": ck, "state": "ready", "episode": 1, "task": task, "port": 25000 + seq[0],
         "url": f"http://127.0.0.1:{25000 + seq[0]}", "validity": {"verdict": "valid"}, "created_epoch": now, "updated_epoch": now,
         "last_code": "", "gates": {}}
    slots[sid] = r
    return r


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def send(self, code, obj, ct="application/json"):
        b = obj if isinstance(obj, bytes) else json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", ct)
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return json.loads(self.rfile.read(n) or b"null") if n else None

    def auth(self):
        if self.headers.get("Authorization") != f"Bearer {A.token}":
            self.send(401, {"error": "token"})
            return False
        return True

    def do_GET(self):
        self.route("GET")

    def do_POST(self):
        self.route("POST")

    def do_DELETE(self):
        self.route("DELETE")

    def route(self, m):
        path = self.path.split("?")[0]
        if path == "/healthz":
            return self.send(200, {"ok": True})
        if not self.auth():
            return
        b = self.body()
        seg = path.strip("/").split("/")
        with lock:
            n = len(live())
            by = {}
            for s in slots.values():
                by[s["state"]] = by.get(s["state"], 0) + 1
        if path == "/v1/health":
            avail = A.mem_total_mib - 2048 - n * A.env_mib
            return self.send(200, {"ok": True, "version": "0.4.2", "image_sha256": "b" * 64, "criu": {"sha256": A.criu_sha, "nested_pidns": True},
                                   "limits": {"vcpus": 4, "memory_mib": 4096, "swap_mib": 0}, "leases": {"defaults": {"env_idle_timeout_s": 1800}},
                                   "checkpoints": [{"id": "osworld-ubuntu", "stale_reason": None}], "slots_by_state": by,
                                   "loadavg": f"{A.load:.2f} 1.00 1.00", "host_mem_available_bytes": int(avail * 1048576),
                                   "admission": {"host": {"mem_total_mib": A.mem_total_mib, "mem_available_mib": avail, "memory_psi_some_avg10": 0.0}, "counts": {"refused": stats["refused"]}},
                                   "fake": stats})
        if path == "/v1/metrics":
            return self.send(200, {"window": "fake", "stats": stats})
        if path == "/v1/metrics/reset":
            return self.send(200, {"reset": True})
        if path == "/openapi.json":
            return self.send(200, {"openapi": "3.0.0"})
        if seg[:2] == ["v1", "tasks"]:
            return self.send(200, [TASK] if len(seg) == 2 else TASK)
        if path == "/v1/checkpoints":
            return self.send(200, [{"id": "osworld-ubuntu", "captured_from": None}] + list(ckpts.values()))
        if seg[:2] == ["v1", "checkpoints"] and len(seg) == 3:
            c = ckpts.get(seg[2]) or ({"id": "osworld-ubuntu", "captured_from": None} if seg[2] == "osworld-ubuntu" else None)
            if not c:
                return self.send(404, {"error": "no checkpoint"})
            if m == "DELETE":
                ckpts.pop(seg[2], None)
            return self.send(200, c)
        if path == "/v1/slots" and m == "GET":
            return self.send(200, list(live()))
        if path == "/v1/slots" and m == "POST":
            with lock:
                if n >= A.max_slots:
                    stats["refused"] += 1
                    return self.send(429, {"error": "overloaded: create refused", "code": "overloaded"})
                stats["creates"] += 1
            time.sleep(A.create_s)
            ck = b.get("checkpoint")
            if ck != "osworld-ubuntu" and ck not in ckpts:
                return self.send(404, {"error": f"no checkpoint {ck}"})
            with lock:
                r = new_slot(ck, b.get("task"))
            return self.send(200, r)
        if seg[:2] == ["v1", "slots"] and len(seg) >= 3:
            s = slots.get(seg[2])
            if not s:
                return self.send(404, {"error": "no slot"})
            if s["state"] == "destroyed":
                return self.send(410, {"error": "slot is destroyed", "closed_reason": "closed"})
            op = seg[3] if len(seg) > 3 else ""
            if op == "" and m == "GET":
                return self.send(200, s)
            if op == "" and m == "DELETE":
                s["state"] = "destroyed"
                stats["deletes"] += 1
                return self.send(200, s)
            if op == "reset":
                time.sleep(A.create_s / 2)
                s["episode"] += 1
                s["last_code"] = ""
                return self.send(200, s)
            if op == "task":
                s["task"] = b["task"]
                return self.send(200, s)
            if op == "step":
                a = b["action"]
                s["last_code"] = a.get("code", "")
                return self.send(200, {"id": s["id"], "episode": s["episode"], "result": {"returncode": 0, "status": "ok"}, "validity": s["validity"]})
            if op == "screenshot":
                return self.send(200, b"\x89PNG\r\n\x1a\n" + b"\0" * 1000, "image/png")
            if op == "gates":
                return self.send(200, {"validity": {"verdict": "valid"}, "gates": {}})
            if op == "verify":
                score = 1.0 if "ORACLE" in s["last_code"] else 0.0
                return self.send(200, {"id": s["id"], "score": score, "valid": True, "checker_error": None, "episode": s["episode"]})
            if op in ("capture", "branch"):
                cid = f"ck{len(ckpts) + 1}-{s['id']}"
                ckpts[cid] = {"id": cid, "captured_from": s["id"], "socket": "/x", "slot": s["num"]}
                if op == "capture":
                    return self.send(200, ckpts[cid])
                kids = []
                with lock:
                    for _ in range(int(b.get("n", 1))):
                        kids.append(new_slot(cid, s["task"]))
                return self.send(200, {"checkpoint": ckpts[cid], "state": cid, "children": kids, "seconds": 0.1})
            return self.send(200, s)
        return self.send(404, {"error": "not found"})


ThreadingHTTPServer.daemon_threads = True
srv = ThreadingHTTPServer(("127.0.0.1", A.port), H)
srv.serve_forever()
