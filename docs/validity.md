# The validity model: what `Reward.valid` means

Drums restores every episode from a checkpoint. A restore can go wrong in ways the desktop does not show: a process
missing, a file lock not restored, the env sharing a namespace with another env, the OOM killer taking an app. So every
episode carries a **gate report**, and a reward is usable for training only when the episode is **valid**.

## Rule

`Reward.valid` is `True` only when all of these hold for the episode the reward came from:

1. every gate below passed (none failed, none still pending);
2. the env was never quarantined during the episode;
3. no task of the env was killed by the OOM killer during the episode (setup, actions, and the checker included);
4. no process of the env died of a crash signal during the episode (`no_app_crash`, below).

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
| `no_app_crash` | added only as **failed**, when a process of the env died of a crash signal (SIGSEGV, SIGILL, SIGBUS, SIGFPE, SIGABRT, SIGTRAP, SIGSYS) since its restore began, or when drumsd cannot vouch for the episode (below); its detail names what crashed | each step, `gates()`/`refresh()`, before verify reads validity, at fork/save, at the episode's end |
| `not_quarantined` | added only as **failed**, when the env was quarantined (by you, a failed gate, a failed save, a failed task setup, or a daemon restart during a save); its detail names the reason | when it is quarantined |

## App crashes (`no_app_crash`)

A desktop can look alive while an app in it is broken: when a Chrome renderer dies, the browser stays up and shows
"Can't open this page", so the process gates and the checker do not see it, and the checker scores the broken page
(a valid 0.0). drumsd watches the kernel's process events (the process events connector, filtered to exits with a
non-zero status) for every exit by a crash signal, attributes it to the cgroup of the dead process (or of its parent,
when the process was already reaped; when both are gone, from a census of the slots' cgroups that drumsd takes every
200 ms), and fails `no_app_crash` for the episode whose tree it was in, and for no other episode:

```python
r = env.verify(strict=False)
r.valid            # False
r.bad_gates        # ["no_app_crash"]
r.invalid_reasons  # ["no_app_crash: chrome (pid 15003) died of SIGILL; a child of chrome (pid 15982) died of SIGILL"]
```

- It covers the restore, the task setup, every action and the checker's own steps.
- A fork or save of an env whose app crashed carries the crash: the saved state is marked, and every env made from it
  starts with `no_app_crash` failed ("made from saved state ... captured after an app crash").
- **A crash no one can place.** A process that lived less than one census pass and died together with its parent
  cannot be attributed. Such a crash counts against every episode whose tree ran Chrome at that moment (the reason
  says "counted against this episode: its tree ran Chrome at that moment"); for the others, verify reports it in
  `crash_not_counted` and the episode stays valid.
- **Fail closed.** The episode is also invalid when drumsd cannot vouch for it: the kernel dropped process events
  (socket overflow), a crash could not be attributed while no census pass was fresh or no slot ran Chrome, the watch
  stopped, or the episode started before the watch did. The last one means: **after a restart of drumsd, the envs it adopts finish their current episode
  invalid** (a crash while drumsd was down would be unseen); their next reset is a new, judged episode.
- An app that exits normally, or is killed (SIGKILL, SIGTERM: what a reset does), is not a crash. A reset or
  destroy freezes the env's cgroup before it kills it, so an app cannot see its peers die and crash on the way out. A crash that is part
  of what an app does on purpose is still a crash here: drumsd cannot tell it apart.
- drumsd refuses to start when it cannot open the watch (it needs root in the host's initial user namespace).
  `"crash_watch": false` in drumsd.json turns it off; `/v1/health` then warns that an env whose app crashed can return
  a valid reward. Do not turn it off on a host whose rewards you train on.
- `/v1/health` → `crash_watch` (running, events seen, unattributed, overflows); `/v1/metrics` →
  `drumsd_app_crashes_total{signal}`.

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
