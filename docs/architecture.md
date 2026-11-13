# Architecture

How drumsd turns one Linux host into many isolated desktops, and what it does on each operation. The user-facing
pages are [validity.md](validity.md), [lifecycle.md](lifecycle.md), [security.md](security.md) and
[reward-audit.md](reward-audit.md); this one is for people reading or changing the daemon.

## Pieces

```text
trainer ── drums-sdk / gateway / MCP ── HTTP+JSON ──> drumsd ──> drums-criu (one template server per checkpoint)
                                                        │    └─> the runtime's Python verbs (capture, provisioning)
                                                        │    └─> OSWorld helper (task setup, checker; unprivileged uid)
                                                        └─> kernel: cgroups v2, namespaces, overlayfs, netns forwarders,
                                                            process events (crash watch, quiescence), /proc
```

| crate | what it holds |
|---|---|
| `drumsd-core` | records and states (`SlotState`, `CheckpointRecord`), the gate definitions and the verdict, the journal, leases, admission control, metrics, quiescence |
| `drumsd-linux` | everything that touches the kernel: cgroups, namespaces, mounts and the overlay layout (`slotfs`), per-env uid ranges (`idrange`), netns forwarders (`proxy`), the CRIU restore client and action-script hook, the crash watch and activity events, retention of destroyed records (`forget`), the opt-in procview (FUSE + BPF) |
| `drumsd` | the daemon: configuration, the axum API, the manager (one state machine per slot, episodes, template servers, reconciliation), the OSWorld bridge (`os_api` serves the Android/Windows pools) |
| `drums-pool` | many drumsd hosts behind one drumsd-compatible API |
| `drumsd-os` | Android and Windows VM pools behind the same API shape (`drumsd serve-os`) |

A slot has the same directory layout, ownership marker, `slot.json` fields, cgroup naming
(`drumsbench-<marker>-pool.scope/drumsbench-<marker>-r{1,2}.scope`) and CRIU command line as a slot made by the
runtime's Python side (`sharedos_rootfs`), so its `capture` verb runs on a drumsd slot unchanged; drumsd uses it for
`capture` and `branch`.

## Checkpoints and template servers

The installer boots the OSWorld desktop once, provisions it (the hardened profile, no privilege escalation for the
desktop user, the guest server) and checkpoints it with drums-criu as the installed image checkpoint
(`osworld-ubuntu`). Every checkpoint, installed or saved, gets a **template server**: a long-lived drums-criu process
that holds the checkpoint's pages. An environment is a `criu restore` against that server, so restored desktops share
the pages they have not written and a restore is a page map rather than a boot. Template servers are started one at a
time, protected from the OOM killer, and adopted from their pidfiles (or restarted) when drumsd starts.

## What a reset does

| step | drumsd |
|---|---|
| stop the old tree | `cgroup.freeze` then `cgroup.kill` on the slot's current leaf cgroup, no wait: the restore goes into the slot's other leaf (`async_kill`) |
| fresh disk state | rename a spare `upper.next`/`work.next` prepared in the background (`warm_upper`); the spare is a copy of the checkpoint's `upper.snap`, shifted to the env's uid range |
| restore | one helper process (`drumsd restore-client`): join the cgroup, unshare the mount namespace, mount the overlay, run `criu restore` against the template server |
| CRIU action script | native `drumsd hook`: returns at once except at pre-resume (bring `lo` up under `netns_fast`, clear soft-dirty bits for incremental capture, put `cpu.max` back after `restore_cpu_boost`) |
| gates | in parallel threads, same definitions as the Python runtime's; a failed gate quarantines the slot |
| netns forwarders | listeners bound once per slot at `port_base + N`; only the target namespace changes |
| guest ready | HTTP probe of the guest server, 5 ms between tries |

Two labelled variants are on by default since v0.2.0: `netns_fast` (`--empty-ns net`, the hook brings `lo` up
instead of CRIU running `ip` and `iptables-restore` for every namespace of the tree) and `restore_cpu_boost`
(`cpu.max` lifted only while every restored task is stopped; the pre-resume hook puts it back before any task runs,
and the cgroup gate proves it). `restore_cpu_weight` raises the slot's `cpu.weight` from restore start to the end of
task setup so a restore on a saturated host is not throttled to 1/N of the CPU.

## Fork, save and quiescence

`capture` dumps a running environment into a new checkpoint. Before the dump the tree must be **quiescent**: no task
leaving, no process or thread starting or exiting for `quiesce.window_s` (proved from the kernel's process events when
they cover the window, else sampled). After the dump, CRIU's seized tree is compared with the quiescent sample and the
source is checked for a process that exited right after; a mismatch rejects the dump and tries again, up to
`quiesce.max_attempts` within `quiesce.timeout_s`, else the capture fails `not_quiescent` and nothing is saved. Dumps
are incremental by default (`--track-mem`, `--prev-images-dir`: only the pages written since the env's last base), with
the parent chain pinned into the new checkpoint as hard links and a full dump every `incremental_max_depth`.

`branch` captures (or reuses a `prefork`) and restores N children, each a new slot with its own uid range, upper layer
and episode. The fork gate in the host tests requires every child to be `ready` and `valid`, a token written in one
child visible in no other, and a child's reset to return it to the branch point.

## Validity

Every create, reset and fork child starts an episode with a gate report: `lock_applied`, `lock_pre_resume`, `cgroup`,
`init_identity`, `mount_table`, `netns`, `guest_ready`, `state_hash`, `isolation`, `alive_after_resume` (judged
`alive_after_s` after the slot answered), plus `no_oom_kill`, `no_app_crash` and `not_quarantined`, which are only ever
added as failed. An episode is `valid` only when every gate passed; `pending` is never valid. A failed gate quarantines
the slot, and a quarantined slot's episode is invalid in every signal (verdict, `verify`'s `valid`, the metrics).

The `no_app_crash` gate comes from the crash watch: every crash-signal exit on the host (SIGSEGV, SIGILL, SIGBUS,
SIGFPE, SIGABRT, SIGTRAP, SIGSYS), read from the kernel's process events connector and attributed to a slot cgroup
through /proc, a 200 ms census of the slot cgroups, or the remembered exit of the parent. A crash none of these can
place fails closed ([SECURITY.md](../SECURITY.md), known limitations).

## Leases, admission, retries

Envs and saved states nobody uses are reaped (`lease` in `drumsd.json`; defaults 1800 s idle, no TTL; per request
`idle_timeout_s`, `ttl_s`; see [lifecycle.md](lifecycle.md)). Every create, capture and fork passes admission control
first: what `MemAvailable` will be after the work in flight must stay above a floor, memory PSI must be below a limit,
and the state tmpfs must keep a minimum free; a refusal is HTTP 429 `overloaded` and nothing is started. Only failures
before any restored task ran are retried (the CRIU detach race, a template refusal that falls back to a private
restore, a template server down that is restarted); every attempt and its seconds are in `cost.attempts`.

## State and crash safety

Slot and checkpoint records live in `journal_dir` (write, fsync, rename, fsync the directory) with an fsynced
`events.jsonl`. A transition is journaled before its work starts. On start drumsd reconciles: a slot found in a
transient state is killed and marked `down` (`quarantined` for an interrupted capture); a `ready` slot whose init is no
longer the recorded process (pid, start time and cgroup) is `down`; a running one is adopted (forwarders re-pointed,
the pending alive gate judged). A checkpoint whose state directory, images, `upper.snap` or image mount is gone is
`state_lost`: stale in the API, a health warning, creates refused with `checkpoint_stale`, and deletable so the
installer can provision it again.

## Retention of destroyed-env records

A destroyed env, a deleted saved state and a rejected dump leave their records (CRIU restore logs at `-v4` are about
20 MB per env) in `forgotten_dir` (default: next to `journal_dir`), in one of two classes:

- `full`: the complete `<name>.tar.zst`, in `<forgotten_dir>/full/`, for envs that ended failed, invalid, down or
  quarantined, that had a failed gate, restore or capture at any point, rejected dumps, and anything drumsd cannot
  classify with certainty. Archives written directly in `<forgotten_dir>` by older versions count as `full`.
- `summary`: for an env that ended valid (closed from `ready`, valid verdict, never quarantined, no failed gate). One
  small `<name>.summary.json` in `<forgotten_dir>/summary/`: `slot.json`, the env's journal record (gates, timings,
  cost), the name, size and sha256 of every file that was dropped, and why it is only a summary.

`retention.mode`, `retention.summary_max_gib`, `retention.full_max_gib` and the older `forgotten_max_gib` are in the
README's configuration table. The record just written is never deleted by its own call. Every deletion is an event
(`forgotten_retention`, with the path, class and size of each file) and is counted in
`drumsd_forget_pruned_total{class}` and `drumsd_forget_pruned_bytes_total{class}`. If a summary cannot be written, the
full archive is written instead; if packing fails, the directory stays on the state dir. `/v1/health` reports
`forgotten.mode`, `forgotten.caps_gib`, `forgotten.full` and `forgotten.summary` (`bytes`, `files`, `pruned_files`,
`pruned_bytes`; `legacy_files` for older archives) and warns when full records exceed `forgotten_warn_gib` or the
filesystem of `forgotten_dir` has less than 10 % free. Metrics: `drumsd_forget_total{class,archived}`,
`drumsd_forgotten_bytes{class}`, `drumsd_forgotten_files{class}`.

## Processes and privilege

drumsd runs as root (cgroups, mount namespaces, CRIU). Everything that runs model-driven code runs as the desktop
user inside the env, in the hardened profile; the OSWorld helper (task setup and checkers) runs as an unprivileged
host uid; a task setup step that needs root is run by drumsd as trusted provisioning through a one-time capability
(`POST /provision/{token}/setup/execute`), inside the slot's user namespace, never by the agent
([security.md](security.md)).

## Subcommands

```text
drumsd serve --config /etc/drums/drumsd.json   the daemon (root)
drumsd serve-os --config /etc/drumsd-os.json   Android and Windows pools only (crates/drumsd-os)
drumsd openapi                                 print the OpenAPI spec
drumsd procview [--mount DIR]                  the opt-in per-env memory/CPU view (root)
drumsd restore-client ...                      internal: one restore
drumsd hook DIR                                internal: the CRIU action script
```

An installed host runs `drumsd serve` as the `drumsd` systemd service, with `drums-boot.service` and
`drums-origin.service` bringing the host back after a reboot (`install/boot.sh`).
