# Drums quickstart

Drums runs isolated Linux desktops (the OSWorld Ubuntu image) as checkpointed process trees on one host. You create
environments, act in them, fork a reached state into copies, and get rewards from OSWorld's checkers, each tagged with
whether every isolation and correctness gate passed.

## Install

Host: a fresh Ubuntu 24.04 x86_64 machine, 16 vCPU / 32 GiB / 120 GiB disk tested (AWS c8i.4xlarge, kernel 7.0.0-1014-aws).

```
tar xzf drums-release-<commit>.tar.gz
sudo ./drums-release-<commit>/install.sh            # about 6-7 minutes on c8i.4xlarge; add --remote to listen on all interfaces
```

It prints the URL and the API token. The token is also in `/etc/drums/token` (root only). The API listens on
127.0.0.1 by default; from your machine, tunnel it: `ssh -L 7070:127.0.0.1:7070 <host>`.

Every input is pinned in `install/lock.json` (apt snapshot and versions, image and file sha256, source commits,
Rust toolchain, a hashed Python lock); the installer stops at the first mismatch. Log: `/var/log/drums-install.log`.

## Use

```
python3 -m venv ~/drums-venv && ~/drums-venv/bin/pip install /opt/drums/sdk/drums-*.whl   # any user; standard library only
export DRUMS_URL=http://127.0.0.1:7070 DRUMS_TOKEN=<token>
```

```python
import drums
c = drums.connect()                                        # DRUMS_URL, DRUMS_TOKEN
with c.create(image="osworld-ubuntu", task="035f41ba") as env:   # OSWorld task id, JSON path, or dict
    env.wait_valid()                                       # this episode's gates passed
    env.step("pyautogui.hotkey('ctrl', 'home')")           # pyautogui code or {"type": "execute", ...}
    obs = env.observe()                                    # obs.png + obs.meta
    kids = env.fork(8)                                     # 8 independent copies of this state
    for k in kids:
        k.step("pyautogui.write('Year_Profit')")
        r = k.verify()                                     # raises InvalidEpisode if a gate failed
        print(k.id, r.score, r.valid)
        k.close()
    state = env.save()                                     # later: c.create(from_state=state); c.delete_state(state) frees it
    print(c.metrics()["cost"])                             # USD per valid episode / per verified reward
```

## What to know

- `verify()` raises `InvalidEpisode` (reward attached, `valid=False`) instead of returning a reward from an episode
  whose gates did not all pass. `verify(strict=False)` returns it flagged.
- Failed restores raise `RestoreFailed` (env down) or `GateFailed` (env quarantined), with the gate report in `.record`.
  drumsd retries only failures that happened before any restored process ran; the SDK retries only GETs while drumsd
  is restarting. Nothing else is retried.
- `fork()`/`save()` are safe right after an action; no sleep is needed. drumsd waits until the env's process tree is
  quiescent (no process or thread starting or exiting for 1 s, no task exiting, no unreaped zombie), dumps it, and
  rejects the dump if CRIU's tree differs from the quiescent one or a dumped process exits right after; then it waits
  and dumps again. If the tree never settles within 30 s it raises `NotQuiescent` (with the unstable processes) and
  captures nothing. `env.wait_quiescent(timeout)` does the wait alone.
- A process that exits silently on a timer just after the dump is always caught, never a valid reward: the dump is redone,
  or the child fails the alive gate. `quiesce.confirm_s` in drumsd.json redoes more such dumps, at that many seconds per capture.
- A killed drumsd is restarted by systemd and reconciles its envs from its journal.
- `c.envs()` lists live envs. Destroyed envs keep their records; `c.env_records(include_destroyed=True)` pages through
  them (`GET /v1/slots?include_destroyed=true&limit=&after=`).
- The desktop runs the image's own login (locale, session environment, settings daemons), as the image's display manager
  would. An installed image's checkpoint made by other runtime sources or another image inspection is stale: drumsd
  refuses envs from it (`CheckpointStale`) until `install.sh` is run again, which provisions the origin anew.
- Metrics: `c.metrics()` and `env.metrics()` (latency per op, episodes by verdict, failures and retries by cause,
  cost from the host price per hour set at install: `--host-usd-per-hour`).
- Examples: `examples/fork_and_verify.py`, `examples/concurrency.py`. API: `api/openapi.json`.
- Not supported yet: more than one host, GPUs, tasks that need the internet inside the desktop, other images.
