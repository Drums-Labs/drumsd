"""Template servers: start, protect, watch and restart them (harness side; the pinned sources are untouched).

One template server (`criu restore --template-server SOCK -D IMAGES`) holds a checkpoint's private memory and serves every
restore of that checkpoint by forking.  If it dies, every later restore of every slot built from it fails, so it must be the
last thing the OOM killer takes, and its death must be seen and repaired, never hidden.

What this module does (reset-failures 20261030T0218Z, cause F of the capture stress: the OOM killer took the template
servers first, at 02:27Z on drums-linux, because each was the largest single process on the host):

  * Own cgroup per server, outside the slots' pool: `drums.slice/drums-template.scope/<name>` (a delegated scope with a keeper,
    like the pool).  The cgroup's memory.min and memory.low are set to the server's memory after it is built (plus headroom),
    and the same totals are set on the scope and the slice, so the protection is effective from the root down (cgroup v2
    protection only holds as far as every ancestor grants it).  Without swap the kernel cannot reclaim the server's anonymous
    memory anyway; the protection then covers its file pages and makes it explicit.
  * oom_score_adj TEMPLATE_OOM_SCORE_ADJ (-900) for the server.  Its forked restores take the client's value (drums-criu
    pageshare fix), and every slot client runs at SLOT_OOM_SCORE_ADJ (+300), so slot processes are the preferred victims.
  * A spec file next to the socket (`<sock>.server.json`): images, criu, pid file, log, cgroup, generation.  `require()`
    checks the server before every template restore: pid alive, really this server (cmdline), socket present.  A dead server
    is restarted from its spec (same images: the hard-linked slots still match its inodes), under a lock so one restart
    serves every waiting slot, and an event line with the evidence (the server cgroup's oom_kill count, the old pid) is
    appended to `template-events.jsonl` in the state dir.  If it cannot be restarted the restore fails with that evidence.
"""

from __future__ import annotations

import fcntl
import hashlib
import json
import os
import subprocess
import time

from .arm import ArmError

CG_ROOT = "/sys/fs/cgroup"
SLICE = "drums.slice"
UNIT = "drums-template.scope"
ROOT = os.path.join(CG_ROOT, SLICE, UNIT)
KEEPER = os.path.join(ROOT, "keeper")

TEMPLATE_OOM_SCORE_ADJ = -900
SLOT_OOM_SCORE_ADJ = 300
HEADROOM = 64 << 20
PROTECT_MAX_FRACTION = 0.5          # never protect more than half the host's memory in total


def _read(p):
    with open(p) as f:
        return f.read().strip()


def _write(p, v):
    with open(p, "w") as f:
        f.write(v)


def mem_total() -> int:
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) * 1024
    raise ArmError("no MemTotal in /proc/meminfo")


def cg_name(sock: str) -> str:
    """One cgroup per server, named after its socket (unique per state dir)."""
    a = os.path.abspath(sock)
    base = os.path.basename(a)
    base = base[:-5] if base.endswith(".sock") else base
    return f"{base}-{hashlib.sha256(a.encode()).hexdigest()[:10]}"


def server_cg(sock: str) -> str:
    return os.path.join(ROOT, cg_name(sock))


def spec_path(sock: str) -> str:
    return sock + ".server.json"


def events_path(sock: str) -> str:
    return os.path.join(os.path.dirname(os.path.abspath(sock)), "template-events.jsonl")


def init_root(run=subprocess.run) -> dict:
    """The delegated template scope with a keeper (so systemd never removes it), memory and pids enabled for its children."""
    if not os.path.isdir(ROOT):
        run(["systemd-run", "--scope", "--quiet", "--unit", UNIT, "--slice", SLICE, "-p", "Delegate=yes", "--",
             "setsid", "-f", "sleep", "infinity"], check=True, stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(100):
            if os.path.isdir(ROOT):
                break
            time.sleep(0.05)
        else:
            raise ArmError(f"systemd-run did not create {ROOT}")
    os.makedirs(KEEPER, exist_ok=True)
    for pid in _read(os.path.join(ROOT, "cgroup.procs")).split():
        _write(os.path.join(KEEPER, "cgroup.procs"), pid)
    sc = _read(os.path.join(ROOT, "cgroup.subtree_control")).split()
    if "memory" not in sc or "pids" not in sc:
        _write(os.path.join(ROOT, "cgroup.subtree_control"), "+memory +pids")
    return {"root": ROOT, "subtree_control": _read(os.path.join(ROOT, "cgroup.subtree_control"))}


def protect_all(run=subprocess.run) -> dict:
    """Sum the servers' memory.min/low and grant the same totals on the scope and the slice (capped)."""
    tot_min = tot_low = 0
    for n in os.listdir(ROOT):
        d = os.path.join(ROOT, n)
        if n == "keeper" or not os.path.isdir(d):
            continue
        try:
            tot_min += int(_read(os.path.join(d, "memory.min")))
            tot_low += int(_read(os.path.join(d, "memory.low")))
        except (OSError, ValueError):
            continue
    cap = int(mem_total() * PROTECT_MAX_FRACTION)
    rep = {"servers_min": tot_min, "servers_low": tot_low, "cap": cap, "capped": tot_low > cap}
    tot_min, tot_low = min(tot_min, cap), min(tot_low, cap)
    for unit in (SLICE, UNIT):
        r = run(["systemctl", "set-property", "--runtime", unit, f"MemoryMin={tot_min}", f"MemoryLow={tot_low}"],
                capture_output=True, text=True)
        if r.returncode != 0:
            raise ArmError(f"systemctl set-property {unit} MemoryMin={tot_min} MemoryLow={tot_low} failed: {r.stderr.strip()[-300:]}")
    rep.update(slice_min=int(_read(os.path.join(CG_ROOT, SLICE, "memory.min"))), scope_min=int(_read(os.path.join(ROOT, "memory.min"))),
               scope_low=int(_read(os.path.join(ROOT, "memory.low"))))
    return rep


def _cmdline(pid: int) -> list:
    try:
        with open(f"/proc/{pid}/cmdline", "rb") as f:
            return [a.decode(errors="replace") for a in f.read().split(b"\0") if a]
    except OSError:
        return []


def _state(pid: int) -> str:
    try:
        with open(f"/proc/{pid}/stat") as f:
            return f.read().rsplit(")", 1)[1].split()[0]
    except (OSError, IndexError):
        return ""


def _oom_kills(cg: str):
    try:
        for line in _read(os.path.join(cg, "memory.events")).splitlines():
            k, v = line.split()
            if k == "oom_kill":
                return int(v)
    except (OSError, ValueError):
        pass
    return None


def load_spec(sock: str):
    try:
        with open(spec_path(sock)) as f:
            return json.load(f)
    except (OSError, ValueError):
        return None


def status(sock: str) -> dict:
    """Is the template server of this socket alive and really serving it?  Read-only: nothing is sent on the socket (the
    accept loop is serial; a probe connection would be one more request)."""
    spec = load_spec(sock)
    out = {"socket": sock, "spec": bool(spec), "alive": False}
    pidf = (spec or {}).get("pidfile") or (sock[:-5] if sock.endswith(".sock") else sock) + ".pid"
    try:
        pid = int(_read(pidf))
    except (OSError, ValueError):
        out["why"] = f"no readable pid file {pidf}"
        return out
    out["pid"] = pid
    cl = _cmdline(pid)
    st = _state(pid)
    if not cl or st in ("Z", "X", ""):
        out["why"] = f"pid {pid} is gone" + (f" (state {st})" if st else "")
    elif "--template-server" not in cl or sock not in cl:
        out["why"] = f"pid {pid} is not the template server of {sock}: {' '.join(cl)[:200]}"
    elif not os.path.exists(sock):
        out["why"] = f"the socket {sock} is gone"
    else:
        out["alive"] = True
        try:
            out["oom_score_adj"] = int(_read(f"/proc/{pid}/oom_score_adj"))
        except (OSError, ValueError):
            pass
    if spec:
        out["generation"] = spec.get("generation")
        out["cgroup"] = spec.get("cgroup")
        out["cgroup_oom_kills"] = _oom_kills(spec.get("cgroup") or "")
    return out


def _gen_name(path: str, gen: int) -> str:
    """The log (or out) file of generation `gen` of a server: the first one's name with -g<gen> before the extension."""
    root, ext = os.path.splitext(path)
    return f"{root}-g{gen}{ext}"


def start(criu: str, images: str, sock: str, log: str, out: str, pidfile: str, generation: int = 1, run=subprocess.run,
          log0: str = None, out0: str = None) -> dict:
    """Start (or restart) the server in its own protected cgroup at TEMPLATE_OOM_SCORE_ADJ; prove both before returning."""
    init_root(run)
    cg = server_cg(sock)
    os.makedirs(cg, exist_ok=True)
    if _read(os.path.join(cg, "cgroup.procs")).split():
        raise ArmError(f"template server cgroup {cg} still holds processes {_read(os.path.join(cg, 'cgroup.procs')).split()}")
    for f in ("memory.min", "memory.low"):
        _write(os.path.join(cg, f), "0")
    if os.path.lexists(sock):
        os.unlink(sock)
    if os.path.lexists(pidfile):
        os.unlink(pidfile)
    t0 = time.monotonic()
    run(["setsid", "-f", "sh", "-c", 'echo "$1" > /proc/self/oom_score_adj && echo $$ > "$0/cgroup.procs" && shift && '
         'exec "$@" </dev/null >"$OUT" 2>&1', cg, str(TEMPLATE_OOM_SCORE_ADJ),
         criu, "restore", "--template-server", sock, "-D", images, "-o", log, "-v4", "--pidfile", pidfile],
        check=True, env={**os.environ, "OUT": out})
    while time.monotonic() - t0 < 120:
        if os.path.exists(sock) and os.path.exists(pidfile):
            break
        time.sleep(0.02)
    else:
        raise ArmError(f"template server did not open {sock} in 120 s: {open(out).read()[-600:] if os.path.exists(out) else ''}")
    pid = int(_read(pidfile))
    adj = int(_read(f"/proc/{pid}/oom_score_adj"))
    if adj != TEMPLATE_OOM_SCORE_ADJ:
        raise ArmError(f"template server {pid} has oom_score_adj {adj}, want {TEMPLATE_OOM_SCORE_ADJ}")
    in_cg = [l[3:] for l in _read(f"/proc/{pid}/cgroup").splitlines() if l.startswith("0::")]
    if in_cg != [cg[len(CG_ROOT):]]:
        raise ArmError(f"template server {pid} is in cgroup {in_cg}, want {cg[len(CG_ROOT):]}")
    cur = int(_read(os.path.join(cg, "memory.current")))
    _write(os.path.join(cg, "memory.min"), str(cur + HEADROOM))
    _write(os.path.join(cg, "memory.low"), str(cur + 2 * HEADROOM))
    prot = protect_all(run)
    spec = {"socket": sock, "images": images, "criu": criu, "pidfile": pidfile, "log": log, "out": out, "cgroup": cg,
            "log0": log0 or log, "out0": out0 or out,
            "generation": generation, "pid": pid, "started_epoch": time.time(), "ready_s": round(time.monotonic() - t0, 3),
            "memory_current": cur, "oom_score_adj": adj}
    tmp = spec_path(sock) + ".tmp"
    with open(tmp, "w") as f:
        json.dump(spec, f)
    os.replace(tmp, spec_path(sock))
    return {**spec, "protection": prot}


def stop(sock: str, run=subprocess.run) -> dict:
    """Stop the server (SIGTERM, then SIGKILL after 5 s), remove its cgroup and its share of the protection."""
    st = status(sock)
    out = {"socket": sock, "was_alive": st["alive"], "pid": st.get("pid")}
    if st["alive"]:
        os.kill(st["pid"], 15)
        for _ in range(100):
            if _state(st["pid"]) in ("", "Z", "X"):
                break
            time.sleep(0.05)
        else:
            os.kill(st["pid"], 9)
    out["gone"] = _state(st["pid"]) in ("", "Z", "X") if st.get("pid") else True
    cg = server_cg(sock)
    if os.path.isdir(cg):
        for _ in range(100):
            if not _read(os.path.join(cg, "cgroup.procs")).split():
                break
            time.sleep(0.05)
        out["cgroup_oom_kills"] = _oom_kills(cg)
        try:
            os.rmdir(cg)
            out["cgroup_removed"] = True
        except OSError as e:
            out["cgroup_removed"] = f"{e}"
        if os.path.isdir(ROOT):
            out["protection"] = protect_all(run)
    if os.path.exists(spec_path(sock)):
        os.replace(spec_path(sock), spec_path(sock) + f".stopped-{int(time.time() * 1000)}")
    return out


def _event(sock: str, rec: dict) -> None:
    with open(events_path(sock), "a") as f:
        f.write(json.dumps(rec, default=str) + "\n")


def require(sock: str, restart: bool = True, run=subprocess.run) -> dict:
    """Before a template restore: the server must be alive.  A dead one is restarted from its spec (once, under a lock shared by
    every slot of this socket) and the event is recorded; without a spec, or if the restart fails, this raises with the evidence."""
    st = status(sock)
    if st["alive"]:
        return {"alive": True, "restarted": False, "pid": st["pid"], "generation": st.get("generation")}
    lock = open(sock + ".lock", "a")
    try:
        fcntl.flock(lock, fcntl.LOCK_EX)
        st = status(sock)                          # another slot may have restarted it while we waited
        if st["alive"]:
            return {"alive": True, "restarted": False, "pid": st["pid"], "generation": st.get("generation"), "waited_for_restart": True}
        spec = load_spec(sock)
        ev = {"epoch": time.time(), "event": "template_server_dead", "socket": sock, "status": st}
        if not spec or not restart:
            ev["action"] = "none (no spec)" if not spec else "none (restart disabled)"
            _event(sock, ev)
            raise ArmError(f"template server for {sock} is dead ({st.get('why')}; cgroup oom_kill={st.get('cgroup_oom_kills')}) "
                           f"and is not restarted: {ev['action']}")
        t0 = time.monotonic()
        g = spec["generation"] + 1
        log0, out0 = spec.get("log0", spec["log"]), spec.get("out0", spec["out"])
        try:
            new = start(spec["criu"], spec["images"], sock, _gen_name(log0, g), _gen_name(out0, g), spec["pidfile"], g, run,
                        log0=log0, out0=out0)
        except (ArmError, OSError, ValueError, subprocess.CalledProcessError) as e:
            ev.update(action="restart failed", error=str(e)[-600:], seconds=round(time.monotonic() - t0, 3))
            _event(sock, ev)
            raise ArmError(f"template server for {sock} is dead ({st.get('why')}; cgroup oom_kill={st.get('cgroup_oom_kills')}) "
                           f"and the restart failed: {e}")
        ev.update(action="restarted", new_pid=new["pid"], generation=new["generation"], seconds=round(time.monotonic() - t0, 3),
                  memory_current=new["memory_current"])
        _event(sock, ev)
        return {"alive": True, "restarted": True, "pid": new["pid"], "generation": new["generation"], "event": ev}
    finally:
        lock.close()


def slot_oom_check(cg: str, init_pid: int) -> dict:
    """After a restore into the slot's cgroup: no slot process may carry the template server's protection.  The init must have
    the slot value (it is 0 in every image, so it takes the client's); any process below 0 means the restore inherited the
    server's oom_score_adj (a CRIU without the pageshare oom fix) and fails closed."""
    vals, bad = {}, []
    for root, _dirs, files in os.walk(cg):
        if "cgroup.procs" not in files:
            continue
        for p in _read(os.path.join(root, "cgroup.procs")).split():
            try:
                v = int(_read(f"/proc/{p}/oom_score_adj"))
            except (OSError, ValueError):
                continue                           # exited meanwhile
            vals[int(p)] = v
            if v < 0:
                bad.append({"pid": int(p), "oom_score_adj": v})
    init = vals.get(init_pid)
    if init is None:
        try:
            init = int(_read(f"/proc/{init_pid}/oom_score_adj"))
        except (OSError, ValueError):
            init = None
    rep = {"init_pid": init_pid, "init_oom_score_adj": init, "processes": len(vals), "negative": bad[:20],
           "values": sorted(set(vals.values()))}
    if init != SLOT_OOM_SCORE_ADJ or bad:
        raise ArmError(f"slot OOM priority gate failed (want init at {SLOT_OOM_SCORE_ADJ}, nothing below 0; template server at "
                       f"{TEMPLATE_OOM_SCORE_ADJ}): {rep}")
    return rep


def oom_kills(cg: str):
    """The slot cgroup's oom_kill counter (hierarchical memory.events: global and cgroup OOM kills of its tasks)."""
    return _oom_kills(cg)
