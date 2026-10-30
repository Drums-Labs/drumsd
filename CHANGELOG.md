# Changelog

## 0.4.2 (SDK 0.2.4), 2026-10-30

A patch release: fixes for what an outside user hit with 0.4.1. Same pins: drums-lab 15e29e4, drums-criu fbbd7c20a,
OSWorld 044bc339.

**Correctness**
- A quarantined env's episode is invalid everywhere. Before, `verify()` returned `valid=False` while the verdict said
  `valid` with no failed gate, strict `verify()` raised a self-contradictory message, the checker still ran, and
  `/v1/metrics` counted the episode as valid. Now the episode gets the failed gate `not_quarantined` (its detail names
  the reason), the verdict is `invalid`, the checker is not run (`score` None, `checker_skipped`), the reward carries
  `invalid_reasons`, and the metrics move a valid episode that is later quarantined (or OOM-killed) to `invalid`.
- `no_oom_kill` also covers the task setup and the time after the last step (from 0.5).

**Cleanup**
- Leases: envs and saved states nobody uses are reaped by drumsd (default 30 min idle; `idle_timeout=` and `ttl=` on
  create, fork and save; `lease` in drumsd.json). Metric `drumsd_lease_reaped_total`, counts in health and metrics.
- The SDK closes the envs a client created when the process exits (atexit), on SIGTERM/SIGHUP (only over default
  handlers), and at the end of `with drums.connect() as client:`. `connect(..., close_on_exit=False)` turns it off.
- `fork()` returns a `ForkResult`: the same list of children, plus `.state` (the saved state it made) and
  `.checkpoint`; also `env.last_state`. `fork(n, keep_state=False)` deletes that state when its last env closes.
  `with env.fork(n) as kids:` closes the children. `env.last_fork` still works and is deprecated.
- Retention (from 0.5): envs that ended valid keep a few-KB summary instead of a full archive (summaries capped at
  2 GiB); failed and invalid ones keep the full archive.
- Template-server logs live in their saved state's directory and leave the RAM-backed state dir with it; leftovers of
  0.4.1 are packed to disk when drumsd starts.
- Reboots (from 0.5): `drums-boot.service` and `drums-origin.service` bring the host back after a stop/start; the
  installer is idempotent and reuses the built CRIU.

**API and SDK**
- Any operation on a destroyed env is HTTP 410 `env_gone` (SDK `EnvGone`, a `NotFound`); `DELETE` on it returns its
  record with 200; `env.close()` on it is a no-op. Wrong-state errors carry `code: "wrong_state"`; unknown ids are 404
  `not_found`.
- `drums.env()` (the 0.1 entry point) defaults to the installed image `osworld-ubuntu` (it was `clean`, which no
  install has).
- Docstrings name both vocabularies: env = slot, saved state = checkpoint, fork = branch, save = capture.

**Install and docs**
- The release tarball ships README, QUICKSTART, host requirements, the action and observation reference, the validity
  model, the lifecycle guide, examples, the OpenAPI spec and this changelog.
- The installer prints a progress line every 30 s during long steps, never prints the API token (it names
  `/etc/drums/token`), and explains that its CRIU sha256 differs per build.
- `/v1/metrics` says that its cost includes idle time since the window started (`cost_note`).

**Not changed**
- No new features beyond these. The per-env resource view (BPF/FUSE) of 0.5 is not included.
