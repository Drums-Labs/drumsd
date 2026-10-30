# Lifecycle and cleanup

Every env holds up to 4 GiB of RAM and 4 vCPUs; every saved state holds about 0.6 GB of the RAM-backed state dir and a
template server. Nothing a trainer forgets may hold the host forever. This page says what is cleaned up, when, and by whom.

## Env states

```
create ──> ready ──step/observe/verify──> ready
             │  └─ fork / save (a saved state is made; the env keeps running)
             ├─ reset ──> ready (a new episode)
             ├─ a gate fails, or quarantine() ──> quarantined ──release()──> down ──reset()──> ready
             └─ close() / lease runs out ──> destroyed (terminal; its records are kept on disk)
```

A create or reset that fails before the desktop ran leaves the env `down` (`RestoreFailed`); `reset()` tries again.

## Who cleans up what

| you | the SDK | drumsd |
|---|---|---|
| `env.close()`, `with client.create(...) as env:` | `with drums.connect() as client:` closes the envs that client created | **leases**: destroys an env nobody used for 30 min (default) |
| `client.delete_state(state)` | at process exit (atexit) and on SIGTERM/SIGHUP, the client closes the envs it created | deletes a saved state with no env made from it for 30 min (default) |
| `env.fork(n, keep_state=False)` | `with env.fork(n) as kids:` closes the children | deletes a `keep_state=False` fork's state when its last env closes |

A `kill -9`, a machine crash or a lost network cannot run any client code; the leases are what cleans up after those.

## Leases

| | default (drumsd.json `lease`) | per request |
|---|---|---|
| env idle timeout | `env_idle_timeout_s`: 1800 | `create(idle_timeout=)`, `fork(idle_timeout=)` for the children |
| env time to live | `env_ttl_s`: none | `create(ttl=)`, `fork(ttl=)` |
| saved state idle timeout | `state_idle_timeout_s`: 1800 | `save(idle_timeout=)`, `fork(state_idle_timeout=)` |
| saved state time to live | `state_ttl_s`: none | `save(ttl=)`, `fork(state_ttl=)` |
| how often drumsd checks | `reap_interval_s`: 15 | - |

`0` (or `null` in drumsd.json) means no limit. Definitions:

- An env is **used** by every API request that names it: step, observe, screenshot, reset, verify, fork, save, a GET of
  its record or gates. Listing envs (`client.envs()`) does not count. An env busy with a long operation (a verify) is
  never reaped mid-operation.
- A saved state is **used** while any env made from it exists. Its idle time counts from when it was made or when its
  last env closed, whichever is later. A state whose TTL ran out is deleted once no env uses it.
- Installed images (`osworld-ubuntu`) are never reaped.

An env's record shows its lease: `env.record["lease"]`, `last_used_epoch`, and `lease_expires_epoch` (if not used again).
A reaped env's record keeps `closed_reason`, e.g. `"lease: idle 1800 s"`; any call on it raises `EnvGone` with that reason.
Counts: `client.health()["leases"]`, `client.metrics()["leases_reaped"]`, and the Prometheus counter
`drumsd_lease_reaped_total{kind="env|state", reason="idle|ttl|children_closed"}`.

A drumsd restart does not reset an env's idle clock (it is journaled within a second).

## Closed and missing envs

- Any operation on a destroyed env raises `drums.EnvGone` (HTTP 410, `code: "env_gone"`). It is a subclass of
  `drums.NotFound`. Replace the env; do not retry.
- `env.close()` on an env that is already gone (closed elsewhere, reaped) is a no-op. `DELETE /v1/slots/{id}` is
  idempotent: it returns the destroyed record with 200.
- An id this drumsd never had is HTTP 404 (`NotFound`, `code: "not_found"`).
- A step on a quarantined env is `Conflict` (409, `code: "wrong_state"`): it still exists; release and reset it, or close it.

## Fork and its saved state

`env.fork(n)` saves the env's state and starts `n` children from it. It returns a `ForkResult`: a list of the ready
children (the same list as before 0.4.2) with `.state` (the saved state, also `env.last_state`), `.checkpoint` (its
record), `.failed` and `.seconds`.

```python
with env.fork(3, keep_state=False) as kids:   # the state is deleted when the last child closes
    ...                                       # leaving the block closes the children
```

Without `keep_state=False`, delete it yourself (`client.delete_state(kids.state)`) or let its lease delete it. A loop that
does neither keeps one state per fork until the lease runs out (~0.6 GB of RAM each).

## Records of closed envs (retention)

A closed env, a deleted state and a rejected dump leave records in `/var/lib/drums/forgotten` (on disk, not RAM):

- `summary/`: a few KB per env that ended valid (its record, gates, timings, a manifest of dropped files). Capped at
  `retention.summary_max_gib` (default 2 GiB), oldest deleted first.
- `full/`: the complete archive (restore logs; ~3 MB compressed) of every env that failed, was invalid or quarantined, and of
  rejected dumps. **Never pruned by default**; set `retention.full_max_gib` to cap it. `/v1/health` warns above
  `forgotten_warn_gib` (50).
- `retention.mode: "keep_all"` keeps a full archive for every env (the 0.4.1 behaviour: ~3 MB per env on disk).

A saved state's template-server logs live in its own directory and leave the RAM-backed state dir with it when it is
deleted (0.4.1 left them behind: ~0.3 MB per fork). On upgrade, drumsd packs any such leftover logs to disk at start.

## Reboots

The state dir is a tmpfs and the image is an nbd mount, so a reboot ends every env and saved state. `drums-boot.service`
mounts them again and `drums-origin.service` re-provisions `osworld-ubuntu` (a few minutes). Until then
`GET /v1/checkpoints` shows `stale: true` with `stale_reason: "state_lost: ..."` and creates fail with `CheckpointStale`.
Records of envs that were live are kept (`down`, reason `state_lost`); close them. Re-running `install.sh` is also safe.

## Metrics windows and cost

`client.metrics()["cost"]` divides the host price by valid episodes and verified rewards over the **current window**.
The window starts when drumsd first started or at the last `client.reset_metrics()`, and its wall time includes idle
time. Call `client.reset_metrics()` at the start of a run to measure that run; the closed window's ledger is kept in
drumsd's event log. The price is the one set at install (`--host-usd-per-hour`, default: the on-demand price of the
instance type, from install/lock.json), not a bill.
