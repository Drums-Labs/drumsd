# Changelog

## RC1, 2026-10-31

Release candidate 1: one build of the pieces that were on separate branches (CHANGES-RC1.md lists every commit), and
no new features beyond the profile switch below.
- **Envs run in the hardened profile by default** ([docs/security.md](docs/security.md)). Each desktop runs in its own
  user namespace (root in it is host uid 100000), with no_new_privs, an 18-capability bounding set, a seccomp blocklist
  and a metadata firewall. drumsd restores such checkpoints on the idmapped image view with `userxattr` and the host
  device binds. A branch gets its origin's directory access. The installer pins `uidmap` and `python3-seccomp` and
  delegates `root:100000:6553600` in `/etc/subuid` and `/etc/subgid`. `--trusted-unhardened` (`"harden": false`) is
  the opt-out, for hosts that run only trusted code. Hardened-by-default and the opt-out are the coordinator's
  recommendation; the owner has not yet approved them.
- **The per-env memory/CPU view (from 0.5) is opt-in:** `--procview` ([docs/procview.md](docs/procview.md)).
- **From 0.5:** one template-server start at a time (concurrent creates after a reinstall failed); per-gate
  state-hash timing; `--swap-mib`.
- **drums-criu rc1:** fix/userns-slot (dump/restore of a slot in its own user namespace) plus fix/pkeys (memory
  protection keys).
- **drums-lab rc1:** harness/reset-failures and harness/oom-inotify (reset, capture and restore failures under load;
  template servers protected from the OOM killer).

## fork-chrome-crash (on daily-use; SDK 0.2.5), 2026-10-31

**Correctness**
- **Fork children restored Chrome without its memory protection keys.** Chrome 130 keeps memory under protection key 1
  (PartitionAlloc thread isolation). CRIU dropped each VMA's key and the mm's allocated keys, so in a restored
  Chrome the next `pkey_mprotect(..., 1)` failed with EINVAL and Chrome crashed on the check (SIGILL, "Can't open this
  page"): every fork child on a multi-field form submit. drums-criu `fix/pkeys` keeps the keys (each VMA's key, the
  allocated keys, PKRU); a plain restore of the same checkpoint crashed the same way before.
- **New gate `no_app_crash`:** a process of the env that dies of a crash signal (SIGSEGV, SIGILL, SIGBUS, SIGFPE,
  SIGABRT, SIGTRAP, SIGSYS) makes the episode invalid with a named reason, from the kernel's process events (attributed
  by cgroup). Fail closed: dropped events, an unattributed crash, a stopped watch, or an episode that started before
  the watch (envs adopted after a drumsd restart finish their current episode invalid) are invalid too. A fork or save
  of a crashed env passes the crash on to every env made from it. `crash_watch` in drumsd.json (default on);
  `/v1/health` `crash_watch`; metric `drumsd_app_crashes_total`.

**Security**
- **No privilege escalation for the agent.** In an env the desktop user is in no administrator group (`sudo`, `admin`,
  `wheel`, `lxd`, `docker`; the image put it in `sudo` and `lxd` with OSWorld's published password), and every
  process it drives has `no_new_privs`. drums-lab `daily-use`; the runtime sources change, so the installer
  re-provisions the image's origin.
- **Trusted provisioning:** a setup or postconfig step whose command runs `sudo` is run by drumsd in the env (as the
  user, with sudo for that step only), never by the agent; a one-time capability per helper call
  (`/provision/{token}/setup/execute`). OSWorld e0df059f's folder is created again.
- Tasks that need the agent itself to act as root are refused with `TaskUnsupported` (422 `task_unsupported`) and
  marked `"supported": false` in `GET /v1/tasks`: 5812b315, 94d95f96, b6781586.
- **`upload_file` reads only under `/var/lib/drums/assets`** (symlinks resolved; `osw.assets_dir`). Before, a task
  could copy any host file the checker user could read (e.g. `/etc/drums/drumsd.json`) into a desktop.

## daily-use (on 0.4.2), 2026-10-31

What an outside lab's own (non-OSWorld) tasks needed.
- **Lab verifiers:** a task's evaluator may be `{"func": "drums_verifier", "verifier": NAME, "function": FN,
  "options": {...}}`. drumsd scores the episode with FN from `/etc/drums/verifiers/NAME.py`: plain Python, run on the
  host as the unprivileged checker user, reading the desktop through `ctx.read_file` / `ctx.run`. Only root-owned files
  that nobody else can write are loaded. A task names a verifier and never carries code. Exceptions and non-numeric
  returns are checker errors, never scores. `info` returns in the reward's `checker` block. Before this, such a task
  failed at setup (`AttributeError: ... no attribute 'drums_verifier'`).
- The installer creates `/etc/drums/verifiers` (root, 0755); `osw.verifiers_dir` in drumsd.json overrides it.
- New doc `docs/custom-tasks.md`: the task format, the setup steps and who runs them, host files for setup, lab
  verifiers, one-time setup via a saved state.

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
  installer is idempotent and reuses the built CRIU. Envs left down or quarantined by the reboot no longer block
  re-provisioning the lost origin (409 "used by envs").

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
