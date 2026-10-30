# drumsd

A control plane for Drums desktop slots. It owns each slot's lifecycle (create, reset, capture, branch, destroy,
quarantine) and replaces the Python/bash glue that drove CRIU around the existing Drums desktop runtime
(`sharedos_rootfs` in drums-lab). CRIU itself (the drums-criu fork, with its template server) and the desktop setup
(`candidate provision/snapshot`, `capture`) are reused, not reimplemented.

```
trainer ── drums (Python client) ── HTTP/JSON ──> drumsd ──> CRIU fork (template server per checkpoint)
                                                    │     └─> existing Python verbs (capture)
                                                    └─> kernel: cgroups, mount namespaces, netns forwarders, /proc
```

## What a reset does

| step | Python path (pageshare reset + proxies verb) | drumsd |
|---|---|---|
| stop the old tree | scoped /proc sweep, SIGTERM, poll, SIGKILL, cgroup.kill backstop | `cgroup.kill` on the slot's current leaf cgroup, no wait (restore goes into the other leaf) |
| fresh disk state | `rm -rf upper work; cp -a upper.snap upper` | rename a spare `upper.next`/`work.next` prepared in the background |
| restore | `sh` -> `unshare` -> `sh mount` -> `python3 pageshare_client.py` -> template server | one helper process: join cgroup, unshare mount ns, mount overlay, same `criu restore` argv over the same socket |
| CRIU action script | `python3 locks.py --hook` at all 7 actions (70-165 ms each) | native `drumsd hook`: returns at once except pre-resume (census in parallel) |
| gates | serial, in Python | in parallel threads; same definitions (ported, tested against the Python formats) |
| netns forwarders | new Python process per port, polled | listeners bound once per slot; only the target namespace changes |
| guest ready | HTTP probe, 1 s sleep between tries | 5 ms between tries |

Optional, off by default, labelled variants: `netns_fast` (`--empty-ns net`, the hook brings `lo` up instead of CRIU
running `ip`/`iptables-restore` six times) and `restore_cpu_boost` (cpu.max lifted only while every restored task is
stopped; the pre-resume hook puts it back before any task runs; the cgroup gate proves it).

## Validity

Every create/reset starts an episode with a gate report. Gates: `lock_applied`, `lock_pre_resume`, `cgroup`,
`init_identity`, `mount_table`, `netns`, `guest_ready`, `state_hash`, `isolation`, `alive_after_resume` (resolves
`alive_after_s` after the slot answered). An episode is `valid` only when every gate passed; `pending` is never valid.
A failed gate quarantines the slot. Only failures before any restored task ran are retried (CRIU detach race, template
refusal -> private restore, template server down -> restart); every attempt and its seconds are in `cost.attempts`.

## State and crash safety

Slot and checkpoint records live in `journal_dir` (write, fsync, rename, fsync dir) with an fsynced `events.jsonl`.
A transition is journaled before its work starts. On start drumsd reconciles: a slot found in a transient state is
killed and marked `down` (`quarantined` for an interrupted capture), a `ready` slot whose init is no longer the
recorded process (pid + start time + cgroup) is `down`, a running one is adopted (forwarders re-pointed, pending alive
gate judged). Template servers are adopted from their pidfiles or restarted.

## Compatibility with the Python runtime

A drumsd slot has the same directory layout, ownership marker, `slot.json` fields, cgroup naming
(`drumsbench-<marker>-pool.scope/drumsbench-<marker>-r{1,2}.scope`) and CRIU command line as a `pageshare branch` slot,
so `sharedos_rootfs.capture` runs on it unchanged (drumsd uses it for `capture`/`branch`).

## API

`api/openapi.json` (also `GET /openapi.json`). Every route except `GET /healthz` needs `Authorization: Bearer <token>`;
the API binds to 127.0.0.1 unless `allow_remote` is set. Main routes: `POST /v1/checkpoints` (adopt), `POST /v1/slots`
(from an image or saved state, optional OSWorld task), `POST /v1/slots/{id}/reset|step|capture|branch|verify|task|
quarantine|release`, `GET /v1/slots/{id}/gates|screenshot|metrics`, `DELETE /v1/slots/{id}`, `GET /v1/health`,
`GET /v1/metrics`, `GET /v1/tasks[/{id}]`, `GET /metrics` (Prometheus).

The Python SDK (`python/drums`, 0.2) and the one-command install are in QUICKSTART.md.

## Build and test

```
cargo build --release
sudo cargo test --release          # 56 unit tests; the forwarder and nested-pidns tests need root
python3 -m unittest python/tests/test_client.py
sudo python3 python/tests/integration_host.py OUT    # against a running daemon on a host with an adopted checkpoint
sudo python3 python/tests/recovery_host.py OUT       # installed host: drumsd killed idle/mid-reset, crashed restore, template server killed
```

Run: `sudo drumsd serve --config drumsd.json` (fields in `crates/drumsd/src/config.rs`). An installed host runs it as
the `drumsd` systemd service (install/install.sh).
