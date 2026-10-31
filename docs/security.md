# Security model of an env

Drums runs model-generated actions inside each env. Treat everything an env does as untrusted.

## The default: the hardened slot profile

Each env's desktop runs in its own Linux namespaces (pid, mount, network, ipc, uts) **and its own user namespace**.
The hardening is set up when the clean checkpoint is provisioned, and every env restored or forked from it keeps it.

| control | what it does |
|---|---|
| user namespace | uid/gid 0..65535 in the env map to the unprivileged host range 100000..165535. Root inside an env owns nothing on the host. Its capabilities count only inside its own namespaces. |
| capability bounding set | Cut to 18 desktop capabilities (`CapBnd 0x28ac65ff`). `CAP_SYS_MODULE`, `CAP_SYS_RAWIO`, `CAP_SYS_BOOT`, `CAP_BPF`, `CAP_PERFMON`, `CAP_NET_ADMIN` and the rest are dropped, so no process in the env can gain them back, even through file capabilities. |
| no_new_privs | Set on the env's init, so every process in the env inherits it. setuid/setgid bits and file capabilities give no extra privilege. |
| seccomp | A default-allow blocklist (`Seccomp: 2`). `init_module`, `finit_module`, `kexec_load`, `bpf`, `perf_event_open`, `keyctl`, `open_by_handle_at` and similar calls return `EPERM`. Chrome's own sandbox still builds under it. |
| metadata firewall | nft rules in the env's network namespace drop traffic to 169.254.0.0/16 (cloud metadata), link-local and host-local ranges. They are loaded before the capability drop, so nothing in the env can change them. |
| network | An env has loopback only, with no route out (see [actions-and-observations.md](actions-and-observations.md)). |

What the installer sets up for this:
- the `uidmap` and `python3-seccomp` packages;
- the line `root:100000:6553600` in `/etc/subuid` and `/etc/subgid`;
- `"harden": true` (the default) in `/etc/drums/rootfs-config.json`.

### Checking it yourself

From the host, for any process of an env (`pgrep -f sharedos_rootfs.tree` gives the env inits):

```
cat /proc/<pid>/uid_map                                # 0 100000 65536
grep -E 'NoNewPrivs|Seccomp|CapBnd' /proc/<pid>/status  # NoNewPrivs 1, Seccomp 2, CapBnd 0000000028ac65ff
```

RC1-VALIDATION.md lists the checks run on the release, from inside an env, with their counts.

## Known limits

- **Every env shares one host uid range.** Envs restored or forked from a checkpoint run in the range of the env the
  checkpoint was made from (100000..165535), because CRIU restores the user namespace map from the checkpoint. Envs are
  still separated from each other by their own pid, mount, network, ipc and uts namespaces and their own upper layers.
  But a process that escaped one env to the host would have the host uid that owns every other env's files.
- **Overlap with the default `ubuntu` delegation.** Ubuntu cloud images give the `ubuntu` account
  `ubuntu:100000:65536` in `/etc/subuid`. That is the same range, so `ubuntu` can map those uids. On a shared host,
  remove that line, or give that account another range.
- **The seccomp filter is a blocklist,** not an allowlist. It lets everything else through, so desktop apps keep working.
- **Kernel bugs** reachable from inside a user namespace remain a risk, as for any container runtime. Keep the host
  kernel patched.

## Opting out: `--trusted-unhardened` (trusted-only hosts)

`sudo ./install.sh --trusted-unhardened` sets `"harden": false`. Envs then run the way they did before the hardening:
- as host root, inside pid, mount, network, ipc and uts namespaces;
- with no user namespace, no capability drop, no no_new_privs, no seccomp filter and no metadata firewall.

**Root in an env is then root on the host.** Use this only on a host that runs only code you trust, such as your own
scripted policies, with no untrusted model actions. The setting is part of the clean checkpoint's provenance: switching
it re-provisions the clean checkpoint. Delete states you saved under the other setting yourself: drumsd does not mark
them stale.

Status: hardened-by-default and this opt-out are the coordinator's recommendation for RC1, and the owner has not yet
approved them.
