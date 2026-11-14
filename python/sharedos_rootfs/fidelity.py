"""Session-environment fidelity: compare a Drums env's process environments with a VM login of the same image.

    python -m sharedos_rootfs.fidelity --port 25002 --vm-dir tests/fixtures/session-env [--out DIR]

reads /proc/PID/environ of the env's gnome-shell and guest server through the env's own guest server (the same calls work on a
VM guest) and compares each with the VM's (vm-gnome-shell.env, vm-server.env, measured on a KVM guest of the pinned image).
Every difference is listed; only the names in EXPECTED are allowed, each with its reason. Exit 1 on any other difference."""
from __future__ import annotations

import argparse
import json
import os
import sys

# Names that differ for a reason that is not a session-setup gap, and why. Everything else must be equal, name and value.
EXPECTED = {
    "INVOCATION_ID": "systemd sets it per unit invocation (the VM runs the session and the server as systemd units)",
    "JOURNAL_STREAM": "systemd: the unit's stdout is the journal",
    "MANAGERPID": "systemd user manager pid",
    "NOTIFY_SOCKET": "systemd user manager notify socket",
    "SYSTEMD_EXEC_PID": "systemd: pid of the unit's main process",
    "SESSION_MANAGER": "gnome-session's XSMP address; Drums runs no gnome-session (its components are started directly)",
    "WINDOWPATH": "gdm: the VT of the X server; the Drums X server has no VT",
    "XAUTHORITY": "gdm: the X cookie file; the Drums X server listens only on the slot's own socket and has no cookie",
    "SSH_AGENT_LAUNCHER": "set by the VM's systemd/gnome-keyring ssh agent launcher, not reproduced (no ssh agent use in the tasks)",
    "LIBGL_ALWAYS_SOFTWARE": "Drums' own: the slot has no GPU device; Mesa uses llvmpipe, as in the GPU-less VM",
    "WERKZEUG_RUN_MAIN": "the guest server's reloader: parent and child differ, either may be measured",
    "WERKZEUG_SERVER_FD": "the guest server's reloader (see WERKZEUG_RUN_MAIN)",
}
PROCS = {"gnome-shell": "pgrep -x -u user gnome-shell | head -1", "server": "pgrep -f server/main.py | tail -1"}


def parse(text: str) -> dict:
    out = {}
    for line in text.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            out[k] = v
    return out


def differences(vm: dict, env: dict) -> dict:
    """{'unexpected': [(name, vm value, env value)], 'expected': [...]}; a value of None means the name is absent."""
    rows = [(k, vm.get(k), env.get(k)) for k in sorted(set(vm) | set(env)) if vm.get(k) != env.get(k)]
    return {"unexpected": [r for r in rows if r[0] not in EXPECTED], "expected": [r for r in rows if r[0] in EXPECTED]}


def read_env(guest, proc: str) -> dict:
    res = guest.execute('tr "\\0" "\\n" < /proc/$(%s)/environ' % PROCS[proc], shell=True, allow_rc=(0, 1))
    return parse(res.get("output") or "")


def main(argv=None) -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", type=int, required=True)
    ap.add_argument("--host", default="127.0.0.1")
    ap.add_argument("--vm-dir", required=True)
    ap.add_argument("--out")
    a = ap.parse_args(argv)
    sys.path.insert(0, os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    from osworld_smoke.guest import Guest  # noqa: E402  (the benchmark's own guest client)
    g = Guest(a.port, a.host)
    rep, bad = {}, 0
    for proc in PROCS:
        vm = parse(open(os.path.join(a.vm_dir, f"vm-{proc}.env")).read())
        d = differences(vm, read_env(g, proc))
        rep[proc] = d
        bad += len(d["unexpected"])
    print(json.dumps(rep, indent=1))
    if a.out:
        json.dump(rep, open(a.out, "w"), indent=1)
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit(main())
