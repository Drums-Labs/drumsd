# Security model of an env

Drums runs model-generated actions inside each env. Treat everything an env does as untrusted.

## The default: the hardened slot profile

Each env's desktop runs in its own Linux namespaces (pid, mount, network, ipc, uts) **and its own user namespace**.
The hardening is set up when the clean checkpoint is provisioned, and every env restored or forked from it keeps it.

| control | what it does |
|---|---|
| user namespace | uid/gid 0..65535 in the env map to an unprivileged host range of 65536 ids, one range per env (below). Root inside an env owns nothing on the host. Its capabilities count only inside its own namespaces. |
| capability bounding set | Cut to 18 desktop capabilities (`CapBnd 0x28ac65ff`). `CAP_SYS_MODULE`, `CAP_SYS_RAWIO`, `CAP_SYS_BOOT`, `CAP_BPF`, `CAP_PERFMON`, `CAP_NET_ADMIN` and the rest are dropped, so no process in the env can gain them back, even through file capabilities. |
| no_new_privs | Set on the env's init, so every process in the env inherits it. setuid/setgid bits and file capabilities give no extra privilege. |
| seccomp | A default-allow blocklist (`Seccomp: 2`). `init_module`, `finit_module`, `kexec_load`, `bpf`, `perf_event_open`, `keyctl`, `open_by_handle_at` and similar calls return `EPERM`. Chrome's own sandbox still builds under it. |
| metadata firewall | nft rules in the env's network namespace drop traffic to 169.254.0.0/16 (cloud metadata), link-local and host-local ranges. They are loaded before the capability drop, so nothing in the env can change them. |
| network | An env has loopback only, with no route out (see [actions-and-observations.md](actions-and-observations.md)). |
| cgroup | Each env is a leaf of its own cgroup v2 slice: `cpu.max`, `memory.max` (and `memory.swap.max`), OOM accounting per env; the `cgroup` gate checks the limits and that every process of the env's pid namespace is inside it. |
| no privilege escalation for the agent | The desktop user is in no administrator group and every process it drives has `no_new_privs`, so `sudo`, `su`, `pkexec`, setuid bits and file capabilities give nothing. A task whose setup needs root runs that step as drumsd's trusted provisioning, never through the agent; the three OSWorld tasks that need the *agent* to act as root are refused (`task_unsupported`, `UNSUPPORTED_AGENT_ROOT` in `crates/drumsd/src/manager.rs`). |

What the installer sets up for this:
- the `uidmap` and `python3-seccomp` packages;
- the line `root:100000:65536000` in `/etc/subuid` and `/etc/subgid` (1000 ranges of 65536 ids: the origin's and one for every env and saved state);
- `"harden": true` (the default) in `/etc/drums/rootfs-config.json`.

### Checking it yourself

From the host, for any process of an env (`pgrep -f sharedos_rootfs.tree` gives the env inits):

```bash
cat /proc/PID/uid_map                                # 0 <base> 65536, a different base per env
grep -E 'NoNewPrivs|Seccomp|CapBnd' /proc/PID/status  # NoNewPrivs 1, Seccomp 2, CapBnd 0000000028ac65ff
```

The host tests (`tests/host/integration_host.py`) check these from inside an env on every release.

## One host uid range per env

CRIU restores a user namespace map from the checkpoint, so without more every env made from one checkpoint would run
in the checkpoint's range and a process that escaped one env would own every sibling's files. drumsd therefore gives
each env its own range (`crates/drumsd-linux/src/idrange.rs`):

- when a slot directory is made, a free range of 65536 ids is taken from the root delegation in `/etc/subuid` and
  `/etc/subgid` (`root:100000:65536000`, written by the installer; ranges outside it are never handed out);
- the env's copy of the checkpoint's upper layer is shifted to that range (file owners, the root id of file
  capabilities, POSIX ACL ids), and it gets an idmapped read-only view of the image for it;
- every restore passes `--userns-remap FROM:TO:65536` to drums-criu, FROM being the range of the checkpoint the images
  come from and TO the env's own;
- the slot directory is `0710 root:<base>`, so only the env's own mapped root group can enter it; a saved state is
  sealed `0700 root:root`, so the env it was captured from cannot reach its files by path;
- a range is held by every slot directory whose plan names it (live envs, saved states, the installed origin), so a
  saved state's range is never reused by a live sibling while the state exists.

An unhardened checkpoint (`--trusted-unhardened`) gets none of this.

## The tenant boundary

Namespaces, cgroups and per-env uid ranges separate envs from each other inside one trusted customer's boundary. They
are not a boundary between customers: **one drumsd host serves one tenant**, and tenants are separated by virtual
machines (the Drums gateway binds each workspace to its own host). Do not put two parties who must not read each
other's data on one drumsd host.

## Known limits

- **A user namespace is not a microVM.** Kernel bugs reachable from inside a user namespace remain a risk, as for any
  container runtime. Keep the host kernel patched; the installer records the kernel each release was tested on (`install/lock.json`, `tested_kernel`) and prints yours next to it.
- **The seccomp filter is a blocklist,** not an allowlist. It lets everything else through, so desktop apps keep working.
- **Overlap with the default `ubuntu` delegation.** Ubuntu cloud images give the `ubuntu` account
  `ubuntu:100000:65536` in `/etc/subuid`, the same ids as the first range of the root delegation, so `ubuntu` can map
  them. On a shared host, remove that line or give the account another range.
- **An unattributed application crash fails closed.** The `no_app_crash` gate attributes every crash-signal exit to
  the env it ran in; a crash in a tree that was already reaped before it could be placed counts against every episode
  that ran Chrome at that moment (or, with no fresh census, every running episode). No reward from a crashed desktop
  is handed out as valid, at the cost of some unrelated episodes marked invalid.
  [Drums-Labs/drumsd#17](https://github.com/Drums-Labs/drumsd/issues/17).
- **The reward audit's scope scan is a signal, not a boundary:** it runs with the desktop user's rights
  ([reward-audit.md](reward-audit.md)).

## Opting out: `--trusted-unhardened` (trusted-only hosts)

`sudo ./install.sh --trusted-unhardened` sets `"harden": false`. Envs then run the way they did before the hardening:
- as host root, inside pid, mount, network, ipc and uts namespaces;
- with no user namespace, no capability drop, no no_new_privs, no seccomp filter and no metadata firewall.

**Root in an env is then root on the host.** Use this only on a host that runs only code you trust, such as your own
scripted policies, with no untrusted model actions. The setting is part of the clean checkpoint's provenance: switching
it re-provisions the clean checkpoint. Delete states you saved under the other setting yourself: drumsd does not mark
them stale.

The hardened profile has been the default since RC1 (v0.2.0); the opt-out is for your own scripted policies on a host
nobody else uses.
