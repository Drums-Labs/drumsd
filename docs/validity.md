# The validity model: what `Reward.valid` means

Drums restores every episode from a checkpoint. A restore can go wrong in ways the desktop does not show: a process
missing, a file lock not restored, the env sharing a namespace with another env, the OOM killer taking an app. So every
episode carries a **gate report**, and a reward is usable for training only when the episode is **valid**.

## Rule

`Reward.valid` is `True` only when all of these hold for the episode the reward came from:

1. every gate below passed (none failed, none still pending);
2. the env was never quarantined during the episode;
3. no task of the env was killed by the OOM killer during the episode (setup, actions, and the checker included).

Train only on rewards with `valid == True`. `verify()` (strict, the default) raises `InvalidEpisode` instead of returning
any other reward; `verify(strict=False)` returns it with `valid=False` and `invalid_reasons`.

`valid` says the episode ran in a correctly restored, isolated desktop. It does **not** say the task was solved: that is
`score` (from OSWorld's checker, usually 0.0 or 1.0). A valid episode with score 0.0 is a valid failure.

## Gates

| gate | what it checks | when |
|---|---|---|
| `lock_applied` | the file locks CRIU applied equal the ones it dumped | before the restored processes run |
| `lock_pre_resume` | the lock census at CRIU's pre-resume step equals the dumped set | before they run |
| `cgroup` | the env's processes are in its own cgroup with its CPU, memory and swap limits | after restore |
| `init_identity` | exactly one init with the env's marker, pid 1 of its own pid namespace | after restore |
| `mount_table` | the init's mount table equals the dumped one | after restore |
| `netns` | the network namespace equals the dumped one (links, addresses, routes) | after restore |
| `guest_ready` | the desktop's guest server answers | after restore |
| `state_hash` | the Desktop and home listing and file hashes equal the checkpoint's | after ready |
| `isolation` | pid, mount, net, ipc and uts namespaces differ from the host's and every other env's | after ready |
| `alive_after_resume` | every restored process is still running 5 s after the env answered (known natural exits excluded) | ready + 5 s |
| `no_oom_kill` | added only as **failed**, when the OOM killer took a task of the env during the episode | each step, and before verify reads validity |
| `not_quarantined` | added only as **failed**, when the env was quarantined (by you, a failed gate, a failed save, a failed task setup, or a daemon restart during a save); its detail names the reason | when it is quarantined |

## Verdicts

`env.validity`, `env.gates()["validity"]` and `Reward.validity` hold the verdict:

| verdict | meaning |
|---|---|
| `pending` | some gate has not resolved yet (normally `alive_after_resume`, for 5 s after create, reset or a fork child). Never valid. |
| `valid` | every gate passed |
| `invalid` | at least one gate failed: `gates` lists them, e.g. `{"verdict": "invalid", "gates": ["not_quarantined"]}` |

All signals agree: `Reward.valid`, the verdict, the failed gates, the reasons, the strict error, and `/v1/metrics`.
For a quarantined env:

```python
env.quarantine("client")
r = env.verify(strict=False)
r.valid            # False
r.verdict          # "invalid"
r.bad_gates        # ["not_quarantined"]
r.invalid_reasons  # ["quarantined: client"]
r.score            # None: the checker does not run on a quarantined env (its state is not trusted)
r.checker_skipped  # "quarantined: client"
env.verify()       # InvalidEpisode: env s147 episode 1 is invalid (quarantined: client); score None is not a usable reward
```

## Timing: the 5 s floor

create, reset and fork children return `state="ready"` with verdict `pending`. The last gate resolves 5 s after the env
answered. `verify()` waits for it. Two consequences:

- The shortest valid episode is about 5 s.
- A process that exits in those 5 s (your first action closes a window, kills an app) cannot be told apart from a bad
  restore, so the episode is flagged invalid. If your first actions may end processes, call `env.wait_valid()` first.

## Quarantine

A quarantined env refuses steps, resets and saves. Its episode is invalid. To reuse it: `env.release()` (kills its
processes; it becomes `down`) then `env.reset()` (a new episode with fresh gates). Or `env.close()` and create another.

## Metrics

`/v1/metrics` (`client.metrics()`) counts episodes once, when their verdict resolves: `usage.episodes.valid`, `invalid`,
and `ended_pending` (reset or closed before the verdict resolved). A valid episode that later becomes invalid
(quarantined, or an OOM kill after the alive gate) is moved from `valid` to `invalid`. `usage.verify.valid` counts
rewards handed out from valid episodes, `flagged_invalid` rewards returned with `valid=False`, `checker_error` checker
failures, and `skipped_quarantined` verify calls on a quarantined env.

## What valid does not cover

- Whether the policy did the task "fairly". `execute` actions run shell commands inside the env (see
  actions-and-observations.md): a policy that may emit them can solve file tasks without touching the GUI.
- Whether the task itself needs the internet: tasks whose setup or checker downloads files need outbound access from
  the host (see host-requirements.md).
