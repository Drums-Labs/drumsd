# Drums release

This directory is a Drums release: the installer, its pinned inputs and the user docs. The version is in `CHANGELOG.md`
and `RELEASE_COMMIT`; the source is [github.com/Drums-Labs/drumsd](https://github.com/Drums-Labs/drumsd).

Drums runs isolated Linux desktops (the OSWorld Ubuntu image) as checkpointed process trees on one host, for
computer-use RL. You create environments, act in them with pyautogui code or shell commands, fork a reached state into
independent copies, and get rewards from OSWorld's checkers. Each reward says whether the episode it came from is
valid: restored correctly, isolated, never quarantined.

## Start here

1. Check the host: [docs/host-requirements.md](docs/host-requirements.md) (Ubuntu 24.04 x86_64, 16 vCPU / 32 GiB /
   150 GiB disk recommended, outbound HTTPS during install).
2. Install and run your first env: [docs/install.md](docs/install.md).

## In this release

| path | what |
|---|---|
| `install.sh` | the installer (`sudo ./install.sh`; `--help` for options) |
| `docs/install.md` | install, connect, a first loop |
| `docs/architecture.md` | how drumsd works: checkpoints, template servers, slots, what a reset does, validity, crash safety |
| `docs/reward-audit.md` | the `audit` block on every reward |
| `docs/host-requirements.md` | OS, CPU, RAM, disk, network, how many envs a host holds |
| `docs/actions-and-observations.md` | screen size, coordinates, action types, who `execute` runs as, screenshots |
| `docs/validity.md` | the gates and what `Reward.valid` means |
| `docs/custom-tasks.md` | your own tasks: setup steps, your app and data, reward functions in Python (lab verifiers) |
| `docs/security.md` | the hardened env profile (default), its limits, and the trusted-only `--trusted-unhardened` opt-out |
| `docs/design/verifier-sandbox.md` | how an uploaded checker is stored by hash and run confined |
| `verifiers/drums_t6.py` | the task factory's lab verifier (sha256 in `install/lock.json`) |
| `docs/procview.md` | the opt-in per-env memory/CPU view (`--procview`) |
| `docs/lifecycle.md` | env states, leases, cleanup, fork states, records, reboots, metrics windows |
| `examples/` | `rl_loop.py` (a minimal leak-free loop), `fork_and_verify.py`, `concurrency.py` |
| `api/openapi.json` | the HTTP API (also served at `GET /openapi.json`) |
| `sdk/` | the Python SDK wheel (standard library only; the same package is `drums-sdk` on PyPI) |
| `CHANGELOG.md`, `docs/release-notes/` | what changed |
| `install/`, `sources/`, `SHA256SUMS`, `RELEASE_COMMIT` | pinned inputs and source bundles the installer checks |

## Names

The SDK and these docs say **env**, **saved state**, **fork** and **save**. The HTTP API and raw records say **slot**
(`/v1/slots`, `"slot": "s104"`), **checkpoint** (`/v1/checkpoints`), **branch** (`POST /v1/slots/{id}/branch`) and
**capture** (`POST /v1/slots/{id}/capture`). They are the same things. An installed image (`osworld-ubuntu`) is a
checkpoint too, one that is never deleted.

## Not supported

GPUs, images other than the pinned OSWorld Ubuntu image, network access from inside the desktop. Many hosts behind
one API is `drums-pool` (in the source repository, experimental), not part of this release's install.
