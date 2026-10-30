# Host requirements

One Drums host runs every env on its own kernel. The installer checks the hard requirements first and stops with a
message if one is not met.

## Required (checked by install.sh)

| | requirement |
|---|---|
| OS | Ubuntu 24.04 (a fresh install; tested with the latest AWS AMI, kernel 7.0.0-1014-aws) |
| CPU | x86_64 |
| cgroups | cgroup v2 (the Ubuntu 24.04 default) |
| RAM | at least 15 GiB |
| disk | at least 70 GiB free on `/` |
| access | root (sudo) |

## Recommended

| | recommendation | why |
|---|---|---|
| instance | AWS c8i.4xlarge (16 vCPU, 32 GiB), us-east-1 | what every release is tested on (on-demand 0.74968 USD/h, us-east-1, 2026-10-01) |
| disk | **150 GiB** gp3 root volume | the install uses 31 GB (the image 23 GB, `/opt/drums` 4.3 GB); records of failed envs (~3 MB each) are kept until you cap them |
| RAM | 32 GiB or more | half of RAM becomes the state tmpfs (saved states live there) |

Measured on c8i.4xlarge, 2026-10-30: after the install `/` had 31 GB used, `/var/lib/drums/state` (tmpfs) 15 GiB.

## Network

**Outbound during the install** (HTTPS unless noted):

| host | for |
|---|---|
| snapshot.ubuntu.com | the pinned apt packages |
| archive.ubuntu.com (HTTP) | one pinned .deb (the Xorg dummy driver) |
| huggingface.co (and its download CDN) | the OSWorld image (11.4 GB zip) |
| github.com | the pinned OSWorld checkout |
| static.rust-lang.org, index.crates.io, static.crates.io | the pinned Rust toolchain and drumsd's locked crates |
| pypi.org, files.pythonhosted.org, download.pytorch.org | OSWorld's checkers (hashed Python lock) |
| 169.254.169.254 | the EC2 instance type, for the default price (optional) |

**Outbound at run time**: only for tasks whose OSWorld setup or checker downloads files (many fetch from
huggingface.co); those run on the host, not in the desktop. The desktops themselves have **no network**: each env has
only a loopback interface and no DNS.

**Inbound**: SSH (22) only. The API listens on 127.0.0.1:7070; reach it with `ssh -L 7070:127.0.0.1:7070 <host>`.
With `install.sh --remote` it listens on all interfaces on port 7070 (plain HTTP, bearer token): open it only to a
private network. The per-env ports (25000+, 27500+, 30000+) are bound to 127.0.0.1.

## How many envs fit

Each env is limited to 4 vCPU (cpu.max) and 4 GiB of memory (memory.max) plus swap. Envs share the image's pages, so
an idle desktop costs much less than its limit. drumsd's admission control refuses a create, fork or save (HTTP 429,
`Overloaded`) when it would leave less than max(8 % of RAM, 4 GiB) available, when memory pressure is high, or when the
state tmpfs would drop below 8 GiB free. Nothing is started then; retry later.

Measured on c8i.4xlarge (32 GiB, 2026-10-30):

| | measured |
|---|---|
| idle env (desktop up, no app opened) | 175-189 MiB `memory.current`; ~225 MiB of host MemAvailable per env; 35 MiB of state tmpfs |
| 40 idle envs at once | all 40 valid; MemAvailable 28.6 -> 19.7 GiB; load average under 5 |
| a saved state (fork or save) | ~0.6 GB of state tmpfs (585 MB freed per delete) |
| 20 envs running Chrome / LibreOffice Calc tasks | 60/60 episodes valid in 3 rounds (runtime-v0.4 readiness chain) |

A guide for one 16-vCPU / 32 GiB host:

- **Light tasks** (file manager, terminal, os-domain tasks): up to ~40 envs; memory is not the limit, CPU is when
  they all act at once.
- **App tasks** (Chrome, LibreOffice, GIMP): plan 1-2 GiB per active env, so 12-20 envs. An app that grows past 4 GiB
  is OOM-killed and its episode is marked invalid (`no_oom_kill`).
- **Saved states**: at most ~10 at once on a 15 GiB state tmpfs (8 GiB must stay free). Use `fork(keep_state=False)`
  or `delete_state` so they do not pile up.
- Watch `client.health()["admission"]` (available memory, floor, refusals) and `state_dir_free_bytes`.

Scale tested elsewhere (runtime-v0.3, r8i.16xlarge, 496 GiB): 128 and 256 envs.
