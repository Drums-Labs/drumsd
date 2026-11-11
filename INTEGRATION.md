# release/v0.2 integration notes

How the SOTA branches were combined into one release, what collided, and what was decided. Pins: `install/lock.json`.

## What is in it (merge order)
| order | branch | PR | what it brings |
|---|---|---|---|
| 1 | `sota/t1-pipefix` (a8d118b) | T1 | `install.sh`: `fs.pipe-user-pages-soft = 0` |
| 2 | `sota/t4` (5e38d55) | #2 | `idrange.rs` (one host uid/gid range per env), `tenantvm/`, 1000-range subuid delegation, portable `make-release.sh` (GNU tar on Linux, bsdtar on macOS) |
| 3 | `sota/t6` (9fbd9b6) via `sota/t6-crash-attr` (b4243c2) | #3, #6 | reward audit on every verify, `no_app_crash` crash attribution by cgroup census, Chrome-only fallback, freeze before `cgroup.kill` |
| 4 | `sota/t2` (06dbfa5) | #4 | events-based quiescence, CRIU `-v3`, `netns_fast` + `restore_cpu_boost` defaults, 128-desktop fixes |
| 5 | `sota/t2b` (fe3291c) | #8 | incremental capture, prefork, restore cpu.weight boost, pidfd wait, resident capture verbs, `max_concurrent_restores` one per 2.7 vCPUs, apt `--allow-downgrades` and no unattended-upgrades |
| 6 | `sota/t3` (d291556) | #7 | `crates/drums-pool` (multi-host pool), `pool/` tooling |
| 7 | `sota/t5` (9b09202) | #5 | `crates/drumsd-os` (Android, Windows pools), `osvm/` |

Companion branches: drums-criu `release/v0.2` (dd8c05657 = drums-main b4fbfddf1 + sota/t4 + sota/t2 + sota/t2b), SDK
`release/v0.2` (Drums-Labs/drums-python-sdk f8124ac = main + sota/t6 + `Env.prefork`), drums-verifiers `release/v0.2`
(sota/t6), monorepo `release/v0.2` (sota/t1, sota/e2b, sota/prime, rl-branch experiments only).

## Conflicts and resolutions (drumsd)
1. **`install/install.sh`, the pipe sysctl** (t1-pipefix, t6, t2, t2b, t3 each added `fs.pipe-user-pages-soft = 0` to
   `/etc/sysctl.d/60-drums-slots.conf` with its own comment). Kept **one** key with one merged comment that cites each
   track's evidence (t6: 12 concurrent LibreOffice forks; t2: 128 desktops; t3: ~250 desktops per host).
   Check: `grep -c 'pipe-user-pages-soft' install/install.sh` is 1.
2. **`install/install.sh`, restore permits and apt** (t2b vs main): t2b's `RESTORES = nproc*3/8, min 4` and
   `--allow-downgrades` + stopping unattended-upgrades are kept; t4's 1000-range subuid line is kept.
3. **`install/lock.json`** (t4 pinned drums-criu sota/t4, t2 sota/t2, t2b sota/t2b and drums-lab sota/t2b; main had
   added a `drums-sdk` source after the SDK split). Pins now: drums-criu `release/v0.2` dd8c05657 (contains all three),
   drums-lab sota/t2b f17644b (archived repo, shipped as a bundle), drums-sdk `release/v0.2` f8124ac. The first
   integration pass had dropped the `drums-sdk` entry; `make-release.sh` reads it, so it is restored.
4. **`crates/drumsd-linux/src/crashwatch.rs`** (t6-crash-attr's cgroup census vs t2b's remembered exits, both for a
   crash whose task and parent were already reaped). Both are kept, in one chain:
   `/proc` task -> `/proc` parent -> census -> census-parent -> remembered exit -> Chrome-only fallback -> none
   (`cgroup_from` names which one placed it; `/v1/health crash_watch.attributed` counts them).
5. **`crates/drumsd/src/manager.rs`** (t4 idrange on restore paths, t6-crash-attr verify/health, t2 quiescence and
   `criu_log_level`, t2b prefork/incremental capture/resident verbs): merged by hand; the capture invocation is t2b's
   resident-verb form and still passes t2's `--dump-log-level` (`criu_log_level.clamp(3, 4)`).
6. **`crates/drumsd-linux/src/cgroup.rs`** (t6-crash-attr freeze-before-kill vs t2b cpu.weight): both kept.
7. **`Cargo.toml`** (t3 adds `drums-pool`, t5 adds `drumsd-os`): members are all five crates.
8. **`python/` and `tests/host/test_hybrid_mcp.py`** (t6, t6-crash-attr, t2b changed the in-tree SDK; main had removed
   `python/` because the SDK lives in Drums-Labs/drums-python-sdk). The deletion stands. t6's SDK changes were already on
   the SDK's `sota/t6`; t2b's `Env.prefork` was not, so it is carried to the SDK's `release/v0.2` (f8124ac) with a test.
   t6's `test_osw_verifier.py` lives in `osw/tests/`; the hybrid/MCP tests in the SDK's `tests/`.

## Conflicts and resolutions (drums-criu)
- `criu/config.c`: sota/t4 (`--userns-remap`) and sota/t2b (`--prev-images-uptime`) both took getopt code 1107.
  `userns-remap` keeps 1107; `prev-images-uptime` is 1108 (option table and `case` label). Nothing reads the number.

## Invariants checked on this branch
- `install.sh` sets `fs.pipe-user-pages-soft = 0` exactly once.
- `make-release.sh` is the portable version (GNU tar `--no-xattrs` on Linux; bsdtar `--no-mac-metadata` on macOS).
- Every `+` line of every branch's diff to `install.sh`, `lock.json`, `make-release.sh`, `manager.rs`, `crashwatch.rs`,
  `cgroup.rs` and `Cargo.toml` is present in the merged file, except comment wording merged in (1), pins superseded in
  (3), and the capture args reshaped in (5).
