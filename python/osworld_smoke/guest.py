"""Client for the public OSWorld guest server (desktop_env/server/main.py at the pinned commit).

Endpoints used: POST /execute {command, shell} -> {status, output, error, returncode};
POST /setup/launch; GET /screenshot -> PNG; POST /screen_size -> {width, height}; GET /platform.
"""

from __future__ import annotations

import hashlib
import json
import time
import urllib.error
import urllib.request

PNG_MAGIC = b"\x89PNG\r\n\x1a\n"


class GuestError(Exception):
    pass


class Guest:
    def __init__(self, port: int, host: str = "127.0.0.1", timeout: float = 130.0):
        self.base = f"http://{host}:{port}"
        self.timeout = timeout
        self.log: list = []  # every request, for the action receipt

    def _req(self, method: str, path: str, payload=None):
        data = None if payload is None else json.dumps(payload).encode()
        req = urllib.request.Request(self.base + path, data=data, method=method,
                                     headers={"Content-Type": "application/json"})
        entry = {"t": time.time(), "method": method, "path": path, "payload": payload}
        self.log.append(entry)
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as r:
                body = r.read()
                entry["status"] = r.status
                return body
        except urllib.error.HTTPError as e:
            entry["status"] = e.code
            raise GuestError(f"{method} {path}: HTTP {e.code}: {e.read()[:300]!r}")
        except (urllib.error.URLError, OSError) as e:
            entry["status"] = None
            raise GuestError(f"{method} {path}: {e}")

    def execute(self, command, shell: bool = False, allow_rc: tuple = (0,)) -> dict:
        """Run in the guest. A returncode outside allow_rc is an error (fail closed)."""
        res = json.loads(self._req("POST", "/execute", {"command": command, "shell": shell}))
        if res.get("status") != "success":
            raise GuestError(f"/execute failed: {res}")
        if res.get("returncode") not in allow_rc:
            raise GuestError(f"guest command rc={res.get('returncode')} stderr={str(res.get('error'))[:300]!r}")
        if not isinstance(res.get("output"), str):
            raise GuestError("guest reply has no string 'output'")
        return res

    def launch(self, command, shell: bool = False) -> None:
        self._req("POST", "/setup/launch", {"command": command, "shell": shell})

    def png_size(self, png: bytes) -> tuple:
        """(width, height) from the IHDR chunk; raises on a malformed PNG."""
        if len(png) < 33 or png[12:16] != b"IHDR":
            raise GuestError("PNG has no IHDR chunk")
        return int.from_bytes(png[16:20], "big"), int.from_bytes(png[20:24], "big")

    def screenshot(self) -> bytes:
        png = self._req("GET", "/screenshot")
        if not png.startswith(PNG_MAGIC):
            raise GuestError("/screenshot did not return a PNG")
        return png

    def screen_size(self) -> tuple:
        res = json.loads(self._req("POST", "/screen_size", {}))
        return int(res["width"]), int(res["height"])

    def platform(self) -> str:
        return self._req("GET", "/platform").decode().strip()


def sha256(b: bytes) -> str:
    return hashlib.sha256(b).hexdigest()
