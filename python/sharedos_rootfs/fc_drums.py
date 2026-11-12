"""Final-comparison helper verbs for the Drums arm (root side; pinned sources and pageshare.py untouched).

    python -m sharedos_rootfs.fc_drums serve --from S --sock PATH [--criu PATH]   one template server per checkpoint (own socket + pid file)
    python -m sharedos_rootfs.fc_drums stop-server --sock PATH
    python -m sharedos_rootfs.fc_drums server-status --sock PATH   alive or not (exit 1), with the server cgroup's oom_kill count
    python -m sharedos_rootfs.fc_drums ensure-server --sock PATH   restart a dead server from its spec (recorded), or fail with the evidence
    python -m sharedos_rootfs.fc_drums proxies --slot N --map HOSTPORT:GUESTPORT[,HOSTPORT:GUESTPORT]   extra netns forwarders (chromium, vlc)
    python -m sharedos_rootfs.fc_drums forget --slot N        after `pageshare drop`: remove the slot dir so its number can be branched again
    python -m sharedos_rootfs.fc_drums info --slot N          slot.json, marker, init pid

pageshare.py's `serve` uses one socket per state dir; the branching protocol needs one live template per reached state next to the clean
one, so `serve` here is the same command line with a socket and pid file named after the checkpoint.  Extra proxies are the arm's own
sharedos_rootfs.proxy module with the slot's tree marker, so the arm's own sweep stops them at every reset/drop like the guest-server proxy."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time

from sharedos_spike import runtime as rt

from . import arm as armmod
from . import launch as L
from .arm import ArmError
from . import tmplsrv
from .pageshare import PageshareArm, pool_init


def _arm():
    from drumsbench.executor import SubprocessExecutor
    cfg = armmod.load_config(os.environ.get("SHAREDOS_ROOTFS_CONFIG", ""))
    return cfg, PageshareArm(cfg, SubprocessExecutor(), rt.Host(module=L.TREE_MODULE), rt.Host(module=L.PROXY_MODULE))


def serve(arm, cfg, origin: int, sock: str, criu: str) -> dict:
    """One template server for this checkpoint, in its own protected cgroup at oom_score_adj -900 (tmplsrv.start)."""
    po = L.slot_paths(cfg["state_dir"], origin)
    pool_init()
    base = sock[:-5] if sock.endswith(".sock") else sock
    log, out, pidf = base + f"-server-{int(time.time())}.log", base + "-server.out", base + ".pid"
    sv = tmplsrv.start(criu, po["images"], sock, log, out, pidf)
    return {"socket": sock, "ready_s": sv["ready_s"], "log": log, "server_pid": sv["pid"], "cgroup": sv["cgroup"],
            "template_cg_memory_current": sv["memory_current"], "oom_score_adj": sv["oom_score_adj"], "protection": sv["protection"]}


def stop_server(sock: str) -> dict:
    out = tmplsrv.stop(sock)
    out["stopped"] = out.get("pid")
    return out


def proxies(arm, cfg, slot: int, mapping: str) -> dict:
    arm.use_slot(slot)
    st = json.load(open(L.slot_paths(cfg["state_dir"], slot)["slot_json"]))
    pid = st["init_pid"]
    p = L.slot_paths(cfg["state_dir"], slot)
    started = []
    for pair in mapping.split(","):
        hp, gp = (int(x) for x in pair.split(":"))
        arm._run(L.proxy_argv(arm._marker(slot), pid, hp, gp, p["proxy_log"] + f".{gp}"), f"start netns proxy {hp}->{gp}", 60)
        started.append([hp, gp])
    return {"slot": slot, "init_pid": pid, "started": started}


def main(argv):
    verb, rest = argv[0], argv[1:]

    def opt(n, d=None):
        return rest[rest.index(n) + 1] if n in rest else d
    try:
        cfg, arm = _arm()
        if verb == "serve":
            print(json.dumps(serve(arm, cfg, int(opt("--from")), opt("--sock"), opt("--criu", "criu"))))
        elif verb == "stop-server":
            print(json.dumps(stop_server(opt("--sock"))))
        elif verb == "ensure-server":
            print(json.dumps(tmplsrv.require(opt("--sock"))))
        elif verb == "server-status":
            st = tmplsrv.status(opt("--sock"))
            print(json.dumps(st))
            return 0 if st["alive"] else 1
        elif verb == "proxies":
            print(json.dumps(proxies(arm, cfg, int(opt("--slot")), opt("--map"))))
        elif verb == "forget":
            slot = int(opt("--slot"))
            if slot == 0:
                raise ArmError("forget refuses the origin slot 0")
            d = L.slot_paths(cfg["state_dir"], slot)["dir"]
            arm.use_slot(slot)
            rep = arm.sweep(slot)
            if not rep["ok"]:
                raise ArmError(f"sweep failed: {rep}")
            keep = os.path.join(os.path.dirname(d), "forgotten", f"slot{slot}-{int(time.time() * 1000)}")
            os.makedirs(os.path.dirname(keep), exist_ok=True)
            # keep the slot's records and logs (never delete raw logs); drop only the bulky state
            for sub in ("upper", "upper.snap", "work", "merged", "images", "criu-root"):
                if os.path.isdir(os.path.join(d, sub)):
                    shutil.rmtree(os.path.join(d, sub))
            shutil.move(d, keep)
            print(json.dumps({"slot": slot, "records_kept_at": keep}))
        elif verb == "info":
            slot = int(opt("--slot"))
            st = json.load(open(L.slot_paths(cfg["state_dir"], slot)["slot_json"]))
            print(json.dumps({"slot": slot, "marker": arm._marker(slot), "argv_marker": arm._argv_marker(slot), "slot_json": st}, default=str))
        else:
            raise ArmError(f"unknown verb {verb}")
        return 0
    except (ArmError, OSError, ValueError, KeyError, subprocess.CalledProcessError) as e:
        print(f"arm error: {type(e).__name__}: {e}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
