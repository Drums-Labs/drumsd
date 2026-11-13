# Security policy

drumsd runs model-generated actions inside desktops on a host you own. This file says which versions get fixes, how
to report a problem, what the security model promises, and what it does not.

## Supported versions

| version | supported |
|---|---|
| the latest `v0.2.x` release on `main` | yes |
| older tags and release candidates (`rc1`, `v0.2.0`) | no: upgrade to the latest release |

A fix ships as a new patch release with a `CHANGELOG.md` entry that names the issue.

## Reporting a vulnerability

Please do not open a public issue for a security problem. Use GitHub's private vulnerability reporting:
[github.com/Drums-Labs/drumsd/security/advisories/new](https://github.com/Drums-Labs/drumsd/security/advisories/new).
Include the release (`RELEASE_COMMIT` of the tarball, or the tag), the host OS and kernel, the install flags, and what
you observed.

- We acknowledge a report within 3 business days and say what we found within 14 days.
- We work with you on disclosure timing; our default is to publish the advisory with the release that fixes it.
- A report that shows an agent gaining privilege on the host, reading another env's state, or getting a `valid` reward
  from a broken desktop is treated as critical.

## What the model promises

Each environment runs in the **hardened profile** by default ([docs/security.md](docs/security.md)):

- its own pid, mount, network, ipc, uts and **user** namespace: uid 0 in the env is an unprivileged host uid, and each
  env gets its own host uid range of 65536 ids (`crates/drumsd-linux/src/idrange.rs`), shifted at restore with
  `--userns-remap`, so no two live envs share one;
- a capability bounding set of 18 desktop capabilities, `no_new_privs` on every process, a seccomp blocklist
  (module loading, `bpf`, `perf_event_open`, `keyctl`, `kexec` and similar return `EPERM`);
- its own cgroup v2 leaf with `cpu.max`, `memory.max` and OOM accounting;
- loopback only, with nft rules that drop traffic to cloud metadata and link-local ranges, loaded before the
  capability drop;
- **the agent never escalates**: the desktop user is in no administrator group, so `sudo`, `su` and `pkexec` refuse.
  A task setup step that needs root is run by drumsd as trusted provisioning, through a one-time capability, never
  by the agent; the OSWorld tasks whose *agent* must be root are refused (`UNSUPPORTED_AGENT_ROOT`).

The API binds to 127.0.0.1 and always needs a bearer token (32+ characters, generated at install, mode 0600, never
printed). `allow_remote` serves plain HTTP: use it only inside a private network, or tunnel.

## What it does not promise

- **Not a boundary between tenants.** Namespaces, cgroups and per-env uid ranges separate environments inside one
  trusted customer's boundary. One drumsd host serves one customer; the Drums gateway binds each workspace to its
  own host, and customers are separated by virtual machines. Do not put two parties who must not read each other's
  data on one drumsd host.
- **Not a microVM.** A kernel bug reachable from inside a user namespace is a risk, as for any container runtime.
  Keep the host kernel patched.
- **The seccomp filter is a blocklist.** It lets everything it does not name through, so desktop applications keep
  working.
- **`--trusted-unhardened`** turns all of the above off: root in an env is then root on the host. It exists for your
  own scripted policies on a host nobody else uses.
- **The reward audit's file-scope scan** runs with the desktop user's rights. It is a reward-hacking signal, not a
  security boundary.

## Known limitations

- **An unattributed application crash fails closed.** The `no_app_crash` gate attributes every crash-signal exit to
  the env it ran in (the task's cgroup, else its parent's, else a 200 ms census of the slot cgroups, else a remembered
  exit of the parent). A crash in a tree that was already reaped before any of these could place it counts against
  every episode that ran Chrome at that moment, or, with no fresh census, every running episode. No reward from a
  crashed desktop is handed out as valid; some unrelated episodes may be marked invalid. Tracked in
  [#17](https://github.com/Drums-Labs/drumsd/issues/17).
- **Fixed in v0.2.1** (PR #13): a concurrent create could fail on a sibling's half-written `service-plan.json`
  ([#11](https://github.com/Drums-Labs/drumsd/issues/11)); the cgroup gate could quarantine a good env when a
  short-lived process exited during its scan ([#12](https://github.com/Drums-Labs/drumsd/issues/12)). Both were
  availability bugs that failed closed, not isolation bugs.
- **Ubuntu's default `ubuntu:100000:65536` subuid line** overlaps the first range of the root delegation the installer
  writes. On a host where the `ubuntu` account is not yours alone, remove that line or move it.

## Scope of this policy

This repository (drumsd and its installer). The CRIU fork is [drums-criu](https://github.com/Drums-Labs/drums-criu);
the Python SDK is [drums-python-sdk](https://github.com/Drums-Labs/drums-python-sdk). A problem in the kernel, in
CRIU upstream, in OSWorld or in the desktop image is out of scope here, but tell us anyway if drumsd makes it worse.
