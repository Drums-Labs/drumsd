#!/usr/bin/env python3
"""Scripted replay "trainer" for the T3 scale test: N concurrent desktops through one DRUMS_URL (the pool), stock SDK.

LABEL: every action is SCRIPTED (a fixed replay of mouse moves, screenshots and, for oracle tasks, a scripted file edit
that solves the task). No model runs. This measures the environment side of RL at scale: create, step, observe,
verify (reward), reset, close; it is not agent throughput.

    python3 replay_trainer.py --url $DRUMS_URL --token-file F --workers 2100 --procs 8 --duration-s 3600 --out DIR

One worker = one desktop slot of concurrency. A worker loops: create(task) -> wait_valid -> [episode]* -> close, with
`--episodes-per-env` episodes per env (reset between them). An episode: `--steps` scripted steps (mouse move, a
screenshot every other step, think time), then the oracle on half the episodes of oracle tasks, then verify(strict=False).

Reward row fields (events-<proc>.jsonl, k="reward"): expected (designed score: 1.0 after the oracle, the task's no-op
score otherwise), score, valid, checker_error, score_ok, valid_reward (valid and no checker error and score == expected),
irv (invalid reward returned as valid: valid and no checker error but score != expected). Every SDK error is a row
(k="error") with its class, HTTP code and the pool's code (episode_lost, host_lost, queue_timeout, ...); the worker then
closes the env (if any) and creates a new one, so a failure never stops the run.

Sampled every 5 s per process (k="tick"): live envs, workers by phase. Seeds are fixed per worker (--seed).
"""
import argparse
import json
import multiprocessing as mp
import os
import random
import sys
import threading
import time

TASKS = {
    # task id: (app, oracle) -- oracles are in-env file edits that make the task's checker return 1.0
    "2ad9387a-65d8-4e33-ad5b-7580065a27ca": ("chrome", "chrome_fav"),
    "9d425400-e9b2-4424-9a4b-d4c7abac4140": ("vs_code", "vscode_wrap"),
    "08c73485-7c6d-4681-999d-919f5c32dcfa": ("thunderbird", "tb_prefs"),
    "93eabf48-6a27-4cb6-b963-7d5fe1e0d3a9": ("chrome", None),
    "70745df8-f2f5-42bd-8074-fbc10334fcc5": ("vs_code", None),
    "734d6579-c07d-47a8-9ae2-13339795476b": ("gimp", None),
    "0a0faba3-5580-44df-965d-f562a99b291c": ("libreoffice_writer", None),
    "035f41ba-6653-43ab-aa63-c86d449d62e5": ("libreoffice_calc", None),
    "04578141-1d42-4146-b9cf-6fab4ce5fd74": ("libreoffice_impress", None),
}
NOOP_SCORE = 0.0   # every task above scores 0.0 untouched (scale-128-256 noop.json, calibrated again before the freeze)

BOOKMARKS_EDIT = r"""
import json, os, subprocess, time
subprocess.run(['pkill', '-x', 'chrome']); time.sleep(2)
p = os.path.expanduser('~/.config/google-chrome/Default/Bookmarks')
d = json.load(open(p)) if os.path.exists(p) else {"roots": {"bookmark_bar": {"children": [], "name": "Bookmarks bar", "type": "folder", "id": "1"},
     "other": {"children": [], "name": "Other bookmarks", "type": "folder", "id": "2"}, "synced": {"children": [], "name": "Mobile bookmarks", "type": "folder", "id": "3"}}, "version": 1}
d.pop('checksum', None)
d['roots']['bookmark_bar']['children'].append({"children": [], "name": "Favorites", "type": "folder", "id": str(100 + len(d['roots']['bookmark_bar']['children'])),
                                      "date_added": "13370000000000000", "date_modified": "0"})
json.dump(d, open(p, 'w'), indent=3)
subprocess.Popen(['google-chrome', '--remote-debugging-port=1337'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
time.sleep(3)
print('ok')
"""
VSCODE_EDIT = r"""
import json, os
p = os.path.expanduser('~/.config/Code/User/settings.json'); os.makedirs(os.path.dirname(p), exist_ok=True)
try:
    d = json.load(open(p))
except Exception:
    d = {}
d['workbench.editor.wrapTabs'] = True
json.dump(d, open(p, 'w'), indent=2)
print('ok')
"""
TB_EDIT = r"""
import os, subprocess, time
subprocess.run(['pkill', '-x', 'thunderbird']); time.sleep(3)
p = os.path.expanduser('~/.thunderbird/t5q2a5hp.default-release/prefs.js')
open(p, 'a').write('user_pref("mail.server.default.applyIncomingFilters", true);\nuser_pref("mail.imap.use_status_for_biff", false);\n')
subprocess.Popen(['/usr/bin/thunderbird'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
for i in range(40):
    if 'Thunderbird' in subprocess.run(['wmctrl', '-l'], capture_output=True, text=True).stdout: break
    time.sleep(0.5)
time.sleep(3)
print('ok')
"""
ORACLES = {"chrome_fav": BOOKMARKS_EDIT, "vscode_wrap": VSCODE_EDIT, "tb_prefs": TB_EDIT}


def errinfo(e):
    body = getattr(e, "body", None)
    return {"cls": type(e).__name__, "status": getattr(e, "status", None), "code": body.get("code") if isinstance(body, dict) else None,
            "msg": str(e)[:600]}


class Out:
    def __init__(self, path):
        self.f = open(path, "a", buffering=1)
        self.lock = threading.Lock()

    def w(self, **kv):
        kv["t"] = round(time.time(), 3)
        line = json.dumps(kv, default=str)
        with self.lock:
            self.f.write(line + "\n")


def worker(a, drums, client, out, wid, stop_at, state):
    rnd = random.Random(a.seed * 100003 + wid)
    tasks = sorted(TASKS)
    env = None
    episodes_on_env = 0

    def timed(op, fn, *args, **kw):
        t = time.monotonic()
        try:
            r = fn(*args, **kw)
            out.w(k="op", op=op, ok=True, s=round(time.monotonic() - t, 4), w=wid, env=getattr(env, "id", None))
            return r
        except Exception as e:  # noqa: BLE001
            out.w(k="op", op=op, ok=False, s=round(time.monotonic() - t, 4), w=wid, env=getattr(env, "id", None), err=errinfo(e))
            raise

    def close():
        nonlocal env
        if env is not None:
            try:
                timed("close", env.close)
            except Exception:  # noqa: BLE001
                pass
        env = None

    # stagger the start so 2,000 creates do not land in the same second
    time.sleep(rnd.uniform(0, a.ramp_s))
    while time.time() < stop_at:
        task = tasks[(wid + episodes_on_env) % len(tasks)] if env is None else env_task
        try:
            if env is None:
                state[wid] = "create"
                env_task = task = tasks[rnd.randrange(len(tasks))]
                env = timed("create", client.create, "osworld-ubuntu", task=task, idle_timeout=a.idle_timeout_s)
                state[wid] = "wait_valid"
                timed("wait_valid", env.wait_valid, timeout=120)
                episodes_on_env = 0
            app, oracle = TASKS[env_task]
            state[wid] = "steps"
            for i in range(a.steps):
                x, y = rnd.randint(200, 1700), rnd.randint(200, 900)
                timed("step", env.step, f"pyautogui.moveTo({x}, {y})", screenshot=(i % 2 == 0))
                time.sleep(rnd.uniform(a.think_min_s, a.think_max_s))
            use_oracle = oracle is not None and rnd.random() < 0.5
            if use_oracle:
                state[wid] = "oracle"
                r = timed("oracle", env.step, {"type": "execute", "command": ["python3", "-c", ORACLES[oracle]], "shell": False}, check=False)
                rc = (r.result or {}).get("returncode")
                if rc != 0:
                    out.w(k="oracle_failed", w=wid, env=env.id, task=env_task, rc=rc)
                    use_oracle = None    # the oracle did not apply: no designed score
            state[wid] = "verify"
            rw = timed("verify", env.verify, strict=False)
            expected = 1.0 if use_oracle else (None if use_oracle is None else NOOP_SCORE)
            score_ok = expected is None or rw.score == expected
            clean = bool(rw.valid and rw.checker_error is None)
            out.w(k="reward", w=wid, env=env.id, task=env_task, oracle=bool(use_oracle), expected=expected, score=rw.score, valid=rw.valid,
                  checker_error=rw.checker_error, bad_gates=getattr(rw, "bad_gates", None), episode=rw.episode, score_ok=score_ok,
                  valid_reward=bool(clean and score_ok and expected is not None), irv=bool(clean and not score_ok),
                  host=(env.record.get("pool") or {}).get("host"))
            episodes_on_env += 1
            if episodes_on_env >= a.episodes_per_env or time.time() >= stop_at:
                state[wid] = "close"
                close()
            else:
                state[wid] = "reset"
                timed("reset", env.reset)
        except Exception as e:  # noqa: BLE001
            out.w(k="error", w=wid, env=getattr(env, "id", None), phase=state.get(wid), err=errinfo(e))
            if isinstance(e, drums.Conflict) and (getattr(e, "body", None) or {}).get("code") == "episode_lost":
                # the pool already recreated this env on a healthy host: a reset continues there
                try:
                    timed("reset_after_failover", env.reset)
                    out.w(k="failover_continue", w=wid, env=env.id, host=(env.record.get("pool") or {}).get("host"))
                    continue
                except Exception as e2:  # noqa: BLE001
                    out.w(k="error", w=wid, env=getattr(env, "id", None), phase="reset_after_failover", err=errinfo(e2))
            close()
            time.sleep(1.0)
    close()
    state[wid] = "done"


def proc_main(a, pi, wids, stop_at):
    sys.path[:0] = [p for p in (a.sdk_path or "").split(":") if p]
    import drums
    tok = open(a.token_file).read().strip()
    client = drums.connect(a.url, tok, timeout=900.0, close_on_exit=True, handle_signals=True)
    out = Out(os.path.join(a.out, f"events-{pi:02d}.jsonl"))
    out.w(k="proc_start", proc=pi, workers=len(wids), sdk=getattr(drums, "__version__", None), url=a.url, args=vars(a))
    state = {}
    th = [threading.Thread(target=worker, args=(a, drums, client, out, w, stop_at, state), daemon=True) for w in wids]
    for t in th:
        t.start()
    while any(t.is_alive() for t in th):
        phases = {}
        for v in list(state.values()):
            phases[v] = phases.get(v, 0) + 1
        out.w(k="tick", proc=pi, alive=sum(t.is_alive() for t in th), phases=phases)
        time.sleep(5)
    out.w(k="proc_end", proc=pi)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", required=True)
    ap.add_argument("--token-file", required=True)
    ap.add_argument("--workers", type=int, required=True)
    ap.add_argument("--procs", type=int, default=8)
    ap.add_argument("--duration-s", type=float, required=True)
    ap.add_argument("--ramp-s", type=float, default=600)
    ap.add_argument("--steps", type=int, default=6)
    ap.add_argument("--think-min-s", type=float, default=1.0)
    ap.add_argument("--think-max-s", type=float, default=3.0)
    ap.add_argument("--episodes-per-env", type=int, default=4)
    ap.add_argument("--idle-timeout-s", type=float, default=900)
    ap.add_argument("--seed", type=int, default=20261108)
    ap.add_argument("--sdk-path", default="")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    os.makedirs(a.out, exist_ok=True)
    stop_at = time.time() + a.duration_s
    json.dump({"args": vars(a), "start": time.time(), "stop_at": stop_at, "tasks": TASKS}, open(os.path.join(a.out, "trainer.json"), "w"), indent=1)
    ps = []
    for pi in range(a.procs):
        wids = list(range(pi, a.workers, a.procs))
        p = mp.Process(target=proc_main, args=(a, pi, wids, stop_at))
        p.start()
        ps.append(p)
    for p in ps:
        p.join()


if __name__ == "__main__":
    main()
