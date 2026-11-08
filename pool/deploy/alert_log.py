#!/usr/bin/env python3
"""Alertmanager webhook receiver: appends every notification (firing and resolved) to a JSON-lines file."""
import json, sys, time
from http.server import BaseHTTPRequestHandler, HTTPServer
out = open(sys.argv[1], "a", buffering=1)
class H(BaseHTTPRequestHandler):
    def do_POST(self):
        b = self.rfile.read(int(self.headers.get("Content-Length") or 0))
        out.write(json.dumps({"t": time.time(), "notification": json.loads(b or b"{}")}) + "\n")
        self.send_response(200); self.end_headers()
    def log_message(self, *a): pass
HTTPServer(("127.0.0.1", 9095), H).serve_forever()
