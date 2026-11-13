# drumsd

A control plane for Drums desktop slots. It owns each slot's lifecycle (create, reset, capture, branch, destroy,
quarantine) and replaces the Python/bash glue that drove CRIU around the existing Drums desktop runtime
(`python/sharedos_rootfs`, imported from drums-lab f17644b). CRIU itself (the drums-criu fork, with its template server) and the desktop setup
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
`alive_after_s` after the slot answered), plus `no_oom_kill` and `not_quarantined`, which are only ever added as failed
(an OOM kill in the episode; a quarantine, with its reason). An episode is `valid` only when every gate passed; `pending`
is never valid. A failed gate quarantines the slot, and a quarantined slot's episode is invalid in every signal (verdict,
verify's `valid`, the metrics). User-facing: docs/validity.md.

## Leases

Envs and saved states nobody uses are reaped (`lease` in drumsd.json; defaults 1800 s idle, no TTL; per request
`idle_timeout_s`, `ttl_s`). See docs/lifecycle.md. Only failures before any restored task ran are retried (CRIU detach race, template
refusal -> private restore, template server down -> restart); every attempt and its seconds are in `cost.attempts`.

## State and crash safety

Slot and checkpoint records live in `journal_dir` (write, fsync, rename, fsync dir) with an fsynced `events.jsonl`.
A transition is journaled before its work starts. On start drumsd reconciles: a slot found in a transient state is
killed and marked `down` (`quarantined` for an interrupted capture), a `ready` slot whose init is no longer the
recorded process (pid + start time + cgroup) is `down`, a running one is adopted (forwarders re-pointed, pending alive
gate judged). Template servers are adopted from their pidfiles or restarted.

## Retention of destroyed-env records

A destroyed env, a deleted saved state and a rejected dump leave their records (CRIU restore logs at -v4 are about 20 MB
per env) in `forgotten_dir` (default: next to `journal_dir`). Each is kept in one of two classes:

- `full`: the complete `<name>.tar.zst`, in `<forgotten_dir>/full/`. For envs that ended failed, invalid, down or
  quarantined, that had a failed gate, restore or capture at any point, rejected dumps, and anything drumsd cannot
  classify with certainty. Archives written directly in `<forgotten_dir>` by older versions count as `full`.
- `summary`: for an env that ended valid (closed from `ready`, valid verdict, never quarantined, no failed gate). One
  small `<name>.summary.json` in `<forgotten_dir>/summary/` (a few KB): slot.json, the env's journal record (gates,
  timings, cost), the name, size and sha256 of every file that was dropped, and why it is only a summary. No logs, no images.

`retention` in drumsd.json (all optional):

| field | default | meaning |
|---|---|---|
| `retention.mode` | `"failures_full"` | `failures_full`: the classes above. `keep_all`: every env keeps a full archive (the old behaviour). |
| `retention.summary_max_gib` | `2` | when summaries exceed this, the oldest summaries are deleted first. `null` turns the cap off. |
| `retention.full_max_gib` | none | when full records exceed this, the oldest full records are deleted first. Unset: failure records are never pruned. |
| `forgotten_max_gib` | none | the older option: a cap over all records of both classes, oldest first. Setting it is an explicit cap, so it can delete failure records and legacy archives. |
| `forgotten_warn_gib` | `50` | `/v1/health` warns when full records exceed this. |

The record just written is never deleted by its own call. Every deletion is an event (`forgotten_retention`, with the
path, class and size of each file) and is counted in `drumsd_forget_pruned_total{class}` and
`drumsd_forget_pruned_bytes_total{class}`. If a summary cannot be written, the full archive is written instead; if
packing fails, the directory stays on the state dir as before.

`/v1/health` reports `forgotten.mode`, `forgotten.caps_gib`, `forgotten.full` and `forgotten.summary` (`bytes`, `files`,
`pruned_files`, `pruned_bytes`; `legacy_files` for older archives) and `forgotten.summaries_pruned`. It warns when full
records exceed `forgotten_warn_gib` (they are never pruned by default; set `retention.full_max_gib` to cap them) and when the
filesystem of `forgotten_dir` has less than 10 % free. Metrics: `drumsd_forget_total{class,archived}`,
`drumsd_forgotten_bytes{class}`, `drumsd_forgotten_files{class}` (and `drumsd_forgotten_bytes{where}`).

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

The Python SDK (`pip install drums-sdk`, import name `drums`) lives in
[Drums-Labs/drums-python-sdk](https://github.com/Drums-Labs/drums-python-sdk); a release pins its commit in
install/lock.json and ships it as `sdk/drums*.whl` (`DRUMS_SDK=path/to/drums-python-sdk install/make-release.sh OUT`,
with DRUMS_CRIU; the steps are in [CONTRIBUTING.md](CONTRIBUTING.md#releases)). Using it and the one-command install are in QUICKSTART.md. The user docs that ship in the
release tarball are QUICKSTART.md, CHANGELOG.md, docs/ (docs/README.md becomes the release's README.md), examples/ and
api/openapi.json.

## Licence

Apache License 2.0 (`LICENSE`, `NOTICE`); third-party crate licences are in `THIRD_PARTY_LICENSES` (generated with
cargo-about from `about.toml` and `about.hbs`). drums-criu stays GPL-2.0 and runs as a separate process; the BPF program is
dual-licensed GPL-2.0-only OR MIT.

## Build and test

```
cargo build --release
sudo cargo test --release          # the forwarder and nested-pidns tests need root
python3 -m unittest install/tests/test_build_wheel.py    # the SDK's own tests are in Drums-Labs/drums-python-sdk
python3 -m unittest discover -s osw/tests
for t in install/tests/test_*.sh; do bash "$t"; done
# installed host, against the running daemon, with the SDK from the release's wheel:
sudo PYTHONPATH=$(ls /opt/drums/sdk/drums-*.whl) python3 tests/host/integration_host.py OUT    # adopted checkpoint `clean`
sudo PYTHONPATH=$(ls /opt/drums/sdk/drums-*.whl) python3 tests/host/recovery_host.py OUT       # drumsd killed idle/mid-reset, crashed restore, template server killed
```

How to contribute, which tests gate a change, and how a release is cut: [CONTRIBUTING.md](CONTRIBUTING.md).

Run: `sudo drumsd serve --config drumsd.json` (fields in `crates/drumsd/src/config.rs`). An installed host runs it as
the `drumsd` systemd service (install/install.sh).
