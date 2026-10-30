# Drums quickstart

Drums runs isolated Linux desktops (the OSWorld Ubuntu image) as checkpointed process trees on one host. You create
environments, act in them, fork a reached state into copies, and get rewards from OSWorld's checkers, each tagged with
whether every isolation and correctness gate passed. Names: the SDK's env, saved state, fork and save are the API's
slot, checkpoint, branch and capture.

## 1. Host

A fresh Ubuntu 24.04 x86_64 machine with cgroup v2 (the default), 16 vCPU, 32 GiB RAM and a 150 GiB disk, with outbound
HTTPS during the install. Tested: AWS c8i.4xlarge in us-east-1 with the latest Ubuntu 24.04 AMI. Details, the network
list and how many envs fit: [docs/host-requirements.md](docs/host-requirements.md).

## 2. Install

```
tar xzf drums-release-<commit>.tar.gz
sudo ./drums-release-<commit>/install.sh          # 7-15 min; a progress line every 30 s
```

At the end it prints the URL (`http://127.0.0.1:7070`) and where the API token is: `/etc/drums/token` (root only, 0600).
The token is never printed. Every input is pinned in `install/lock.json` and checked; the first mismatch stops the
install. Log: `/var/log/drums-install.log`. Running `install.sh` again is safe (it reuses what is already there).

The API listens on 127.0.0.1. From your laptop, tunnel it: `ssh -L 7070:127.0.0.1:7070 <host>`. `--remote` binds all
interfaces instead, over plain HTTP with only the bearer token: use it only inside a private network.

## 3. Connect

```
python3 -m venv ~/drums-venv && ~/drums-venv/bin/pip install /opt/drums/sdk/drums-*.whl     # any user; no dependencies
export DRUMS_URL=http://127.0.0.1:7070 DRUMS_TOKEN=$(sudo cat /etc/drums/token)
```

```python
import drums
with drums.connect() as client:                         # DRUMS_URL, DRUMS_TOKEN; closes this client's envs at the end
    client.reset_metrics()                              # cost from now on (the window otherwise includes idle time)
    with client.create(task="e0df059f") as env:         # an OSWorld task id (8+ characters), a JSON path, or a dict
        env.wait_valid()                                # this episode's gates passed (5 s after ready)
        obs = env.observe()                             # obs.png (1920x1080 PNG), obs.meta (instruction, ...)
        env.step("pyautogui.hotkey('ctrl', 'alt', 't')")   # pyautogui code, or an action dict
        with env.fork(4, keep_state=False) as kids:      # 4 independent copies; their saved state is deleted with them
            for k in kids:
                k.step({"type": "execute", "command": ["ls", "Desktop"]})
                r = k.verify(strict=False)              # the task's checker
                print(k.id, r.score, r.valid, r.invalid_reasons)
        state = env.save()                              # later: client.create(from_state=state)
        client.delete_state(state)                      # or let its lease delete it (30 min unused)
    print(client.metrics()["cost"])                     # USD per valid episode / verified reward, this window
```

A longer loop: `examples/rl_loop.py`. All actions and what a screenshot is: [docs/actions-and-observations.md](docs/actions-and-observations.md).

## 4. What to know

- **Train only on `Reward.valid`.** `verify()` raises `InvalidEpisode` instead of returning a reward from an invalid
  episode; `verify(strict=False)` returns it with `valid=False` and `invalid_reasons`. A quarantined env is always
  invalid. [docs/validity.md](docs/validity.md).
- **The first 5 s of an episode.** create, reset and fork children are `ready` with verdict `pending`; the last gate
  resolves 5 s later. An action that ends a process in those 5 s makes the episode invalid: call `env.wait_valid()` first.
- **Cleanup.** Envs and saved states unused for 30 min are reaped by drumsd (leases; `idle_timeout=`/`ttl=` on create,
  fork, save; 0 = never). The SDK also closes the envs a client created at exit, on SIGTERM/SIGHUP and at the end of a
  `with` block. `fork()` returns the children plus `.state`; `keep_state=False` deletes it with them.
  [docs/lifecycle.md](docs/lifecycle.md).
- **Errors are typed.** `RestoreFailed` (env down; `reset()` retries), `GateFailed` (env quarantined), `TaskSetupFailed`,
  `ActionFailed` (the action ran and exited non-zero; `step(..., check=False)` returns it), `EnvGone` (the env was closed
  or reaped: replace it), `Overloaded` (the host is full: retry later), `NotQuiescent` (fork/save found no stable
  moment in 30 s; nothing was saved), `CheckpointStale` (re-run the installer). drumsd retries only failures before any
  restored process ran; the SDK retries only GETs while drumsd restarts.
- **fork() and save() are safe right after an action.** drumsd waits until the process tree is quiescent and checks the
  dump against it.
- **Metrics.** `client.metrics()` and `env.metrics()`: latency per operation, episodes by verdict, failures and retries by
  cause, rewards, and a cost from the host price per hour set at install (`--host-usd-per-hour`).
- **Reboots.** `drums-boot.service` and `drums-origin.service` bring the host back after a stop/start (a few minutes);
  envs that were running are gone.
- **API.** `api/openapi.json` (also `GET /openapi.json`). Every route except `GET /healthz` needs `Authorization: Bearer <token>`.
