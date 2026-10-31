# Per-env memory and CPU view (opt-in)

By default an env reads the host's `/proc/meminfo`, `/proc/cpuinfo`, `/proc/stat`, `/proc/swaps`, the
`/sys/devices/system/cpu/{online,possible,present}` lists and `sysinfo(2)`. So apps that size themselves from total
memory see the whole host, not the env's 4 GiB limit. GIMP is one: it sizes its tile cache with `sysconf(_SC_PHYS_PAGES)`.

`sudo ./install.sh --procview` turns on a per-env view:
- **Files.** drumsd serves those files from the env's own cgroup limits and usage, through a read-only FUSE file system
  (`drums-procview.service`, mounted at `/run/drums/procview`). The files are bound read-only into each env.
- **sysinfo(2).** A BPF program rewrites the memory fields of `sysinfo(2)` for env processes only.

Measured in runtime-v05 (r8i.16xlarge, GIMP a746add2, 32 envs at a time, 4 GiB + 3,220 MiB swap each):
- with the view: 96/96 episodes succeeded, with 0 OOM kills;
- without it: 67/96, with GIMP OOM-killed in 29.

Why it is off by default: it adds a FUSE daemon and a BPF program to every host. Its security review passed with
conditions. And without it, the measured app suite already passes on the 4 GiB default with no swap.

What it does not cover:
- `nproc`, `sched_getaffinity` and `SC_NPROCESSORS_CONF` still report the host's CPU count;
- uptime, loadavg, vmstat and the PSI files are still the host's;
- with kernel lockdown, the BPF program is refused, so `sysinfo(2)` shows the host. The files still serve, and
  `/v1/health` warns.

Changing the setting re-provisions the clean checkpoint, because the setting is part of its provenance.
`--no-procview`, or installing again without `--procview`, turns the view off.
