"""Bring-up of the clean checkpoint (run by install.sh as root; standard library only).

1. Slot 0 is provisioned cold from the read-only image (`sharedos_rootfs.candidate provision`): overlay root, namespaces,
   cgroup, Xorg (dummy driver), GNOME, the OSWorld guest server.
2. Once the desktop is ready, Nautilus is activated over D-Bus and the slot is checkpointed at once (`candidate snapshot`),
   as the final comparison's ensure_origin does (the Nautilus activation and the dump must be within a second or two).
3. drumsd is started and adopts slot 0 as the image checkpoint (template server; the reference state hash is taken from
   the running origin). drumsd records the origin's provenance (runtime sources and image inspection digests); on a re-run
   whose sources or inspection changed (e.g. new login facts) the stale checkpoint is deleted and slot 0 provisioned again.
4. Smoke test: one slot from the checkpoint must come up with every gate passed (alive gate included), then is closed.
Every step's seconds go to --record.
"""
from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request


def guest(port, method, path, body=None, timeout=30):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(f"http://127.0.0.1:{port}{path}", data=data, method=method, headers={"Content-Type": "application/json"})
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return r.read()


def sh(port, cmd):
    return json.loads(guest(port, "POST", "/execute", {"command": cmd, "shell": True}))


def api(url, token, method, path, body=None, timeout=900):
    data = None if body is None else json.dumps(body).encode()
    req = urllib.request.Request(url + path, data=data, method=method,
                                 headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"})
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            return json.loads(r.read() or b"null")
    except urllib.error.HTTPError as e:
        raise RuntimeError(f"{method} {path}: HTTP {e.code} {e.read()[:500]!r}") from None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--criu-dir", required=True)
    ap.add_argument("--lab", required=True)
    ap.add_argument("--rootfs-config", required=True)
    ap.add_argument("--drumsd-config", required=True)
    ap.add_argument("--image-id", required=True)
    ap.add_argument("--record", required=True)
    a = ap.parse_args()
    rec = {"steps": []}

    def step(name, t0, **kw):
        rec["steps"].append({"step": name, "seconds": round(time.monotonic() - t0, 3), **kw})
        json.dump(rec, open(a.record, "w"), indent=1)
        print(f"bringup: {name} {rec['steps'][-1]['seconds']} s", flush=True)

    dcfg = json.load(open(a.drumsd_config))
    token = open(dcfg["token_file"]).read().strip()
    url = "http://" + dcfg["listen"].replace("0.0.0.0", "127.0.0.1")
    env = dict(os.environ, SHAREDOS_ROOTFS_CONFIG=a.rootfs_config, PYTHONPATH=a.lab, PYTHONDONTWRITEBYTECODE="1",
               OSWORLD_READY_DISPLAY_PROCS="Xorg", PATH=f"{a.criu_dir}:/usr/sbin:/usr/bin:/sbin:/bin")

    def verb(module, *args, timeout=900):
        r = subprocess.run([sys.executable, "-m", module, *args], cwd=a.lab, env=env, capture_output=True, text=True, timeout=timeout)
        if r.returncode != 0:
            raise RuntimeError(f"{module} {' '.join(args)} rc={r.returncode}: {(r.stderr or '')[-1500:]}")
        return r

    def drumsd_up():
        try:
            return json.loads(urllib.request.urlopen(url + "/healthz", timeout=2).read()).get("ok")
        except Exception:  # noqa: BLE001
            return False

    # already brought up (re-run of the installer): drumsd has the checkpoint
    subprocess.run(["systemctl", "start", "drumsd"], check=False)
    # drumsd answers once it has reconciled its journal; with thousands of env records (a host that ran 256 envs for
    # hours) that took more than the 10 s this used to wait, and the re-run then tried to provision slot 0 again
    for _ in range(600):
        if drumsd_up() or subprocess.run(["systemctl", "is-active", "--quiet", "drumsd"]).returncode != 0:
            break
        time.sleep(0.2)
    if drumsd_up():
        have = {c["id"]: c for c in api(url, token, "GET", "/v1/checkpoints")}
        c = have.get(a.image_id)
        if c is not None and not c.get("stale"):
            print(f"bringup: checkpoint {a.image_id} already adopted", flush=True)
            return 0
        if c is not None:
            # made by other runtime sources or another image inspection (an upgrade whose login facts changed), or its
            # state is gone (`state_lost`: the host was restarted, the state tmpfs and the image mount with it): the
            # origin is provisioned again from the image; refused by drumsd while an env made from it still exists
            t0 = time.monotonic()
            print(f"bringup: checkpoint {a.image_id} is stale ({c.get('stale_reason')}): re-provisioning the origin", flush=True)
            d = api(url, token, "DELETE", f"/v1/checkpoints/{a.image_id}?installed=true")
            step("delete_stale_origin", t0, reason=c.get("stale_reason"), old_provenance=c.get("provenance"), deleted=d)
    subprocess.run(["systemctl", "stop", "drumsd"], check=False)

    port = 25000
    t0 = time.monotonic()
    verb("sharedos_rootfs.candidate", "provision", "--slot", "0")
    step("provision_slot0", t0)
    t0 = time.monotonic()
    err = None
    while time.monotonic() - t0 < 300:
        try:
            if guest(port, "GET", "/platform", timeout=5).decode().strip() == "Linux" and sh(port, "pgrep -x gnome-shell >/dev/null && echo up").get("output", "").strip() == "up" \
                    and guest(port, "GET", "/screenshot", timeout=20).startswith(b"\x89PNG"):
                break
        except Exception as e:  # noqa: BLE001
            err = str(e)
        time.sleep(1)
    else:
        raise RuntimeError(f"origin desktop not ready after 300 s: {err}")
    time.sleep(5)      # let the session settle (the final comparison sampled processes 2 s apart here)
    step("desktop_ready", t0)
    t0 = time.monotonic()
    r = json.loads(guest(port, "POST", "/execute", {"command": ["gdbus", "call", "--session", "--dest", "org.freedesktop.DBus", "--object-path",
                                                               "/org/freedesktop/DBus", "--method", "org.freedesktop.DBus.StartServiceByName", "org.gnome.Nautilus", "0"],
                                                   "shell": False}))
    verb("sharedos_rootfs.candidate", "snapshot", "--slot", "0")
    step("nautilus_and_snapshot", t0, nautilus=str(r.get("output"))[:200])

    t0 = time.monotonic()
    subprocess.run(["systemctl", "enable", "--now", "drumsd"], check=True)
    for _ in range(300):
        if drumsd_up():
            break
        time.sleep(0.2)
    else:
        raise RuntimeError("drumsd did not answer /healthz: journalctl -u drumsd")
    step("drumsd_start", t0)
    t0 = time.monotonic()
    c = api(url, token, "POST", "/v1/checkpoints", {"id": a.image_id, "slot": 0})
    step("adopt", t0, checkpoint=c)

    t0 = time.monotonic()
    s = api(url, token, "POST", "/v1/slots", {"checkpoint": a.image_id})
    if s.get("state") != "ready":
        raise RuntimeError(f"smoke slot is {s.get('state')}: {s.get('last_error')}")
    while True:
        g = api(url, token, "GET", f"/v1/slots/{s['id']}/gates")
        if g["validity"]["verdict"] != "pending":
            break
        if time.monotonic() - t0 > 120:
            raise RuntimeError("smoke slot: alive gate still pending after 120 s")
        time.sleep(0.5)
    api(url, token, "DELETE", f"/v1/slots/{s['id']}")
    if g["validity"]["verdict"] != "valid":
        raise RuntimeError(f"smoke slot not valid: {json.dumps(g)[:1500]}")
    step("smoke_slot_valid", t0, slot=s["id"], timings=s.get("timings"))
    return 0


if __name__ == "__main__":
    sys.exit(main())
