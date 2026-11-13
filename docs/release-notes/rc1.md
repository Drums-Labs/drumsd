# Release notes: RC1 (2026-10-31)

What Drums RC1 was made of, kept for the record (this file was `CHANGES-RC1.md` at the repository root until v0.2.2).
RC1 is one build: drumsd `rc1`, drums-lab `rc1`, drums-criu `rc1` and drums-verifiers tag `rc1`, all private
repositories at the time. drumsd's `install/lock.json` pins the exact drums-lab and drums-criu commits. Every pass in
the internal validation record was measured on the release tarball built from these heads, and from no other branch.

| repo | rc1 head | base |
|---|---|---|
| drumsd | the commit that holds this file (the release's `RELEASE_COMMIT`); listed through `81663924d` | runtime-v0.4.2 `3841ee705` (released internally) |
| drums-lab | `cb3145d92` | fix/session-fidelity `15e29e437` (= v0.4.2's pin; app-compat 0f6b2b6 + the image's own login) |
| drums-criu | `b4fbfddf1` | drums-main `358380f66` |
| drums-verifiers | tag `rc1` -> `4e6f51e61` | main (the prime-rl adapter: turn cap, admission retry, cleanup) |

**Coordinator decisions in RC1, labelled as such; the owner has not yet approved them:**
- the HARDENED slot profile is the default, and `--trusted-unhardened` (`"harden": false`) is a documented opt-out for trusted-only hosts;
- the licence is Apache-2.0 for our code and GPL-2.0 for the CRIU fork. Another worker adds the licence files; RC1 does not depend on them.
- The per-env memory/CPU view (BPF + FUSE) is OPT-IN (`--procview`). Its security review passed with conditions.

**Fixes found while building RC1** (not on any source branch) are marked `[rc1]` below.

## drumsd (`rc1`)

Merges into rc1, in order:
- `828431a` merge: daily-use 688ec49 (lab reward-function hook, custom-tasks docs) into rc1
- `5186413` merge: runtime-v0.5 e87d3d6 into rc1 (per-env view, template start lock, gate timing)
- `957b010` merge: daily-use a9465c2 (agent privilege removal, provisioning, no_app_crash and crash-taint gates) into rc1

### From daily-use (688ec49, then a9465c2)

Why: an outside lab's own tasks need a reward-function hook (688ec49). The agent must never get admin: setup steps that need root go to drumsd's provisioning, which enters the slot's user namespace on the hardened profile. Crash-validity: `no_app_crash` makes an episode invalid when an env process dies of a crash signal, and an env made from a crash-tainted state comes up invalid. Pins fix/pkeys (the restored-Chrome crash).

- `688ec49` lab verifiers: a task can be scored by a root-installed Python function (drums_verifier), run on the host as the checker user and reading the desktop through its guest server; installer creates /etc/drums/verifiers; docs/custom-tasks.md; tests
- `4a82eac` gates: no_app_crash, and setup steps that need root run as drumsd's provisioning
- `1656f11` osw: sudo steps go to provisioning; upload_file reads only the assets directory; SDK TaskUnsupported
- `9c309f3` lock: drums-criu fix/pkeys b2643c09c (memory protection keys across C/R), drums-lab daily-use 4343e68 (no privilege escalation in a slot); changelog
- `ec6f922` gates: an env made from a crash-tainted state comes up invalid (no_app_crash, inherited) instead of quarantined: the restore is sound, the reward says why it is unusable
- `dfed82d` provisioning: a restore's task setup names its new tree (init pid, cgroup) in the capability; the slot record has it only after the setup (e0df059f's sudo step got 409)
- `9fc8da9` provisioning: in a slot with its own user namespace (hardened profile) the step enters it too, so it runs as the slot's mapped ids, never host root
- `aa88902` tests: recovery_host reads the gate list inside the gates report
- `a9465c2` tests: recovery_host A resets e2, so B compares e2 with its new tree

### From runtime-v0.5 (e87d3d6), the commits v0.4.2 had not cherry-picked

Why: the per-env memory/CPU view (procview: GIMP 96/96 vs 67/96 at 32 envs), which RC1 ships as OPT-IN. Also one template-server start at a time (concurrent creates after a reinstall failed 9/12), per-gate state-hash timing, and --swap-mib. v0.5's retention, OOM-validity, reboot and lost-checkpoint commits are already in v0.4.2 as cherry-picks; git lists them again below because their patch ids differ.

- `2fe9a50` forget: classify records as full or summary, separate layout, caps per class, summaries with a manifest of dropped files
- `bdb0281` forget: manager classifies records (full vs summary) from the closed env's state, gates and a failure mark; per-class metrics, pruning events and health fields; config: retention block
- `335f4b2` docs: retention of destroyed-env records
- `50621d4` procview: the slot's own memory, swap and CPU view rendered from its cgroup only (meminfo, cpuinfo, stat, swaps, cpu lists), refused for any non-slot reader
- `c6e9d2d` procview: read-only FUSE file system over /dev/fuse serving the views per reading task, connection kept in the systemd fd store across restarts
- `f310a0b` procview: BPF fexit program on sysinfo(2) gives slot callers the slot's memory, swap and task count (glibc's _SC_PHYS_PAGES), link pinned across restarts
- `2c37da4` procview: drumsd procview subcommand (mount or take back the view, attach the sysinfo program, serve); restore externals from a dump's mount table
- `93a5403` restore: re-bind the per-env view at every restore of a checkpoint that holds it; swap_mib per env; health reports the view and warns when it is down
- `1cc8a30` install: drums-procview.service before drumsd, the view dir in both configs, --swap-mib; pin drums-lab runtime-v0.5
- `28bf642` Cargo.lock: aya and its dependencies
- `0399e75` install: --no-procview, a labelled A/B control that provisions the origin without the per-env view and stops it
- `9544537` template: one server start at a time; concurrent creates after the servers were stopped (a reinstall) failed with 'template server cgroup still holds processes' instead of waiting for the first start
- `1246a43` state-hash gate: one event per gate with its seconds, each guest request's seconds, guest-ready time and the host's load then (drumsd threads, memory and CPU PSI, load, admission in flight)
- `567f7ba` provenance: the view setting is part of an origin's provenance (an origin made with the other setting is stale and re-provisioned); install: --no-procview masks the view for the boot
- `a69a2a8` recover: a checkpoint whose state dir, images, upper.snap or image mount is gone is state_lost: stale in the API, a health warning with ok false, creates refused with checkpoint_stale; live envs of a lost host end down with the reason
- `d54e0c1` install: --no-procview removes the view's unit (a runtime mask cannot override /etc)
- `346b205` recover: a checkpoint whose state is lost can be deleted (so the installer can re-provision it) while envs of it are only on record as down
- `681837f` install: boot.sh and drums-boot/drums-origin units re-mount the state tmpfs, re-attach the verified image and re-provision the origin after a reboot; image attach cleans a stale mount record and nbd device
- `2a88e1b` install: install.sh installs boot.env, boot.sh and the boot units, attaches through the stale-record cleanup, reuses the CRIU built from the pinned commit; bringup treats a lost origin as stale; quickstart section on reboots
- `188e00c` procview: without the sysinfo program (no BTF/fexit, kernel lockdown) the file views still serve; a status file says why and health warns
- `47bd286` procview: a refused sysinfo program is reported with the verifier's reason, not its whole log
- `8f385e6` gates: no_oom_kill also covers the task setup (baseline taken before it) and the time after the last step (checked after the checker, before the reward's validity is read); GIMP OOM kills came back valid with score 0.0
- `cd3507b` tests: oom_host, an OOM kill during setup or after the last step makes the episode invalid (no_oom_kill); fails on 47bd286, passes after
- `2a9bbb1` procview: a rendering is reused for 100 ms per slot and file, and the view's unit is capped at 2 CPUs, so an env that floods its view cannot take host CPU outside its cpu.max (measured: ~8 CPUs before, 2 after)
- `e87d3d6` recover: a lost checkpoint can be deleted past quarantined envs too (115 down and quarantined envs from before a reboot of drums-scale blocked the boot unit's re-provisioning)

### [rc1] commits made while building RC1

Why: each one resolves a merge at its root, or makes the hardened default work under drumsd (it had run only under the lab harness before).

- `c179a4b` rc1: run envs in the hardened slot profile by default (drums-lab rc1), --trusted-unhardened opts out
- `3589ed7` docs: security.md (the hardened slot profile, its limits, the trusted-only opt-out), procview.md (the opt-in per-env view)
- `5ac1032` rc1: drop what the runtime-v0.5 merge added twice (RetentionConfig and its test, blocks_checkpoint_delete: cherry-picked into 0.4.2 at other places); provenance test compares against the current inspection
- `efd5026` slotfs: a branch gets its origin's access to the slot and images directories (a hardened slot restores in its own user namespace, where CRIU reads the images as the slot's mapped root; drumsd's 0700 root-only dirs failed every restore with 'Unable to open tty-info.img: Permission denied'); test
- `cf72484` rc1 docs: CHANGES-RC1.md (every merged commit and why), the RC1 changelog entry, README and QUICKSTART on the hardened default and the opt-in view; the release ships and requires CHANGES-RC1.md, docs/security.md and docs/procview.md
- `c6aedae` tests: oom_host case A accepts an env that is already invalid when it is handed out (no_oom_kill named; since no_app_crash a setup-time OOM can also kill Chrome with SIGTRAP before wait_valid); lock: drums-lab rc1 cb3145d
- `8166392` CHANGES-RC1.md: regenerated for the final rc1 heads

## drums-lab (`rc1`)

Merges into rc1, in order:
- `fcd07e7` merge: harness/oom-inotify into rc1
- `ecb1da1` merge: sec/slot-hardening ed0bc53 into rc1
- `c813e61` merge: f62cc5f into rc1
- `b9d8500` merge: runtime-v0.5 a75d1d7 (per-env view binds) into rc1
- `e19909e` merge: daily-use-hardened 94b4187 (daily-use 4343e68: no privilege escalation in a slot, assets dir; with sec/slot-hardening) into rc1

### harness/reset-failures (a11d712) and harness/oom-inotify (1e38321)

Why: reset, capture and restore failures under load, each with a test that fails before and passes after (1,000-reset stress: 2 failures before, 0 after; a restorer fault fails in ~5 ms instead of hanging). Also: template servers in their own OOM-protected cgroup, the per-episode OOM count, and a restore that hits the inotify limit names the host setting.

- `9448238` launch: a thread that exits while the cgroup readback walks its process retries the snapshot instead of failing the reset
- `211030c` runtime: terminate waits (bounded) for a SIGKILLed init to finish exiting, and a process reaped before the signal is gone, not an error
- `7d5babb` arm: a failed criu dump or restore reports the Error lines of its -o log
- `bf36a66` capture: a failed capture frees its capture slot instead of blocking every later capture of the parent
- `a11d712` sweep: a tree that survives the per-pid SIGKILL is killed through its own cgroup (cgroup.kill), or the slot fails closed
- `b19e889` tmplsrv: template servers in their own protected cgroup, watched and restarted
- `f66ccc1` pageshare_client: a missing or dead template server fails at once
- `c35e3c6` pageshare, fc_drums: protected servers, slots as preferred OOM victims, per-episode OOM count
- `d362cc9` tests: template server restart, OOM-priority gate, fast client failure, episode OOM count
- `921dffe` pageshare: a reset retried after a failed restore does not repeat the episode's OOM count
- `c2d0beb` host: 60-drums-slots.conf raises the shared inotify limits, with the reason and the measurement
- `1e38321` arm: a restore that hit the inotify limit names the host setting

### sec/slot-hardening (ed0bc53): the hardened slot profile

Why: before this, slot root was host root (identity uid map, full capabilities, no seccomp). Each slot now gets its own user namespace, no_new_privs, the capability allowlist, seccomp and the metadata firewall. It includes the harness fix for the child-17 SIGKILL (a sweep never acts on a replaced slot).

- `8db071f` sec: run each desktop slot in its own user namespace with seccomp, dropped caps and a metadata firewall
- `6294185` sec: record the userns install deps found during live Linux verification
- `de0a1e0` sec: make a slot in its own user namespace start, dump and restore
- `cb8826e` tests: slot in its own user namespace (device binds, mount contract, external keys, userxattr, host access, idmapped lower)
- `c61a7e1` sec: say that a restored slot keeps the uid range of its checkpoint
- `ed0bc53` pageshare: a sweep never acts on a slot that was replaced, and empties the pool cgroup of a hung template restore

### daily-use (f62cc5f, then 4343e68 via daily-use-hardened 94b4187)

Why: the Acme lab task set (own web app and spreadsheet, own setup and Python reward functions) for the daily-use loop. And no privilege escalation for the desktop user: no administrator group (sudo, admin, wheel, lxd, docker), no_new_privs on every process it drives. Assets come only from drumsd's assets directory.

- `f62cc5f` daily-use: Acme lab task set (web app, spreadsheet, own setup and Python reward functions), scripted policy, runners and the restored-Chrome repros
- `d5e29ee` sharedos_rootfs: the desktop user gets no privilege escalation in a slot
- `ed802c2` daily-use: Acme assets live under drumsd's assets directory (/var/lib/drums/assets/acme); upload_file reads nothing outside it
- `4343e68` sharedos_rootfs: lxd and docker are administrator groups too (root through the daemon's socket); the OSWorld image puts the user in lxd

### runtime-v0.5 (a75d1d7): the view binds

Why: the slot side of the opt-in per-env view (read-only binds, the mount contract, CRIU externals).

- `73ce636` sharedos_rootfs: the per-env memory/CPU view binds are the slot's only external mounts: allowed by the contract only read-only at their exact place, named to criu at dump and re-bound from the view at restore
- `809b3b5` sharedos_rootfs: the slot init binds the drumsd procview files read-only over /proc/{meminfo,cpuinfo,stat,swaps} and the cpu online/possible/present lists when the config names a view; a source that is not the live view refuses the slot
- `a75d1d7` tests: view binds in the mount contract, dump and restore externals, tree steps, a non-FUSE source refused

### [rc1] commits made while building RC1

- `2de95e0` harden: the hardened slot profile stays the default; "harden": false is the trusted-only opt-out
- `cb3145d` tests: the OCI reference bundle's user has the image's groups minus the administrator ones (4343e68 drops sudo, gid 27); the expectation still had it (rc1 full-suite run on Linux: 875/876)

The conflict resolutions are in the merge commits' messages. pageshare.sweep keeps both backstops, and runs no cgroup.kill before the replaced-slot check. CRIU external mounts now come from the slot's own mount table, so the device binds of a hardened slot and the view binds of a view slot are each named exactly when they exist.

## drums-criu (`rc1`)

`rc1` = fix/pkeys-userns, a fast-forward chain on drums-main 358380f66; there are no merge commits. Pushed only to the private `drums` remote; nothing went upstream.

### fix/ip6tables (fbbd7c20a)

Why: a dump that failed before taking the network lock no longer unlocks rules it never took (no stray 'Bad rule' iptables/ip6tables errors).

- `fbbd7c20a` net: a dump unlocks only the network lock it took

### fix/userns-slot (a17b78241)

Why: dump and restore of a slot in its own user namespace. A nested task's groups were read from the wrong user namespace, and the dumpable flag was lost in call_in_child_process(), so slot root could not open /proc fdinfo at restore. Each fix has a zdtm test that fails before and passes after.

- `549eedbc4` dump: read the groups of a nested task from the user namespace of the root task, by its real pid
- `9ab683f62` zdtm: run userns_chrome01 in the uns flavor too (the root task in its own user namespace)
- `7cb53173f` util: keep the dumpable flag of the caller of call_in_child_process()
- `a17b78241` zdtm: userns_chrome01: a /proc dir of the zygote kept open, and the oom_score_adj of a renderer

### memory protection keys (fix/pkeys-userns b4fbfddf1)

Why: the root cause of the restored-Chrome crash in fork copies. CRIU dropped each VMA's protection key and the mm's allocated keys, so PartitionAlloc's next pkey_mprotect failed and Chrome died with SIGILL. pkey01 fails 9/9 before and passes 9/9 after, including the uns flavour.

- `dfb7ae3fa` images: the protection key of a VMA and the keys an mm has allocated
- `ad88c44d2` compel: add pkey_mprotect, pkey_alloc and pkey_free on x86
- `f100f93c0` dump: keep memory protection keys
- `52ac59456` restore: give back memory protection keys
- `b8859f65e` zdtm: add pkey01, memory protection keys across C/R
- `b4fbfddf1` zdtm: run pkey01 in the uns flavor too (the root task in its own user namespace, as a hardened slot)

## drums-verifiers (tag `rc1`)

The tag points at main `4e6f51e`: `7585cf8` (the adapter that daily-use and prime-rl-adapter validated) plus `4e6f51e`, which removes the sudo password from the ScaleCUA prompt. The prompt is a modification of Apache-2.0 SCALE-CUA text, and the file header marks it as one. The adapter's last commits:

- `1d476d8` soak: --hard-stop-utc (no iteration starts after it, a running one is killed at it and marked partial), so a shared model endpoint is released on time
- `83a2d26` inject: slot number from the record's num (fork child ids are s<n>-<k>, found in a mock soak where the crashed restore never fired)
- `bd276e8` latency_probe: a shared model endpoint's latency during a soak (one short request every 30 s, vLLM running/waiting/KV gauges)
- `e993cb1` close envs that never came up: a create that comes back down (RestoreFailed) and failed fork children were never closed and stayed in drumsd's live list as down slots (found in the real-model soak: crashed restore of the control parent s245-2); test fails without the fix. refused_calls.py: second, labelled verdict exempting only tool calls the adapter refused as invalid actions (count must equal invalid_actions)
- `7585cf8` cleanup survives a drumsd restart: env close and fork-state delete are retried while drumsd is unreachable (up to 120 s; the SDK never retries a DELETE), a cleanup that still fails is recorded and fails the acceptance check (found in the real-model soak: a fork group's closes fell in the 1.28 s restart after the injected SIGKILL; 2 ready + 1 down slots and the fork state s418-c422 were left behind); test fails without the fix
- `4e6f51e` scalecua: the system prompt no longer gives the model the image's sudo password (a marked modification of the Apache-2.0 SCALE-CUA text; in a Drums env the desktop user has no privilege escalation and an agent is never handed a credential); test

## Not in RC1

- drums-lab branches not listed above: research, density and benchmark harnesses, agent-loop, exp3.
- runtime-v0.5's default-on view: in RC1 the view is opt-in.
- No new features beyond the `harden` switch, which the hardened-default decision needs.
