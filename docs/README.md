# Drums 0.4.2

Drums runs isolated Linux desktops (the OSWorld Ubuntu image) as checkpointed process trees on one host, for
computer-use RL. You create environments, act in them with pyautogui code or shell commands, fork a reached state into
independent copies, and get rewards from OSWorld's checkers. Each reward says whether the episode it came from is
valid: restored correctly, isolated, never quarantined.

## Start here

1. Check the host: [docs/host-requirements.md](docs/host-requirements.md) (Ubuntu 24.04 x86_64, 16 vCPU / 32 GiB /
   150 GiB disk recommended, outbound HTTPS during install).
2. Install and run your first env: [QUICKSTART.md](QUICKSTART.md).

## In this release

| path | what |
|---|---|
| `install.sh` | the installer (`sudo ./install.sh`; `--help` for options) |
| `QUICKSTART.md` | install, connect, a first loop |
| `docs/host-requirements.md` | OS, CPU, RAM, disk, network, how many envs a host holds |
| `docs/actions-and-observations.md` | screen size, coordinates, action types, who `execute` runs as, screenshots |
| `docs/validity.md` | the gates and what `Reward.valid` means |
| `docs/custom-tasks.md` | your own tasks: setup steps, your app and data, reward functions in Python (lab verifiers) |
| `docs/lifecycle.md` | env states, leases, cleanup, fork states, records, reboots, metrics windows |
| `examples/` | `rl_loop.py` (a minimal leak-free loop), `fork_and_verify.py`, `concurrency.py` |
| `api/openapi.json` | the HTTP API (also served at `GET /openapi.json`) |
| `sdk/drums-0.2.5-py3-none-any.whl` | the Python SDK (standard library only) |
| `CHANGELOG.md` | what changed |
| `install/`, `sources/`, `SHA256SUMS`, `RELEASE_COMMIT` | pinned inputs and source bundles the installer checks |

## Names

The SDK and these docs say **env**, **saved state**, **fork** and **save**. The HTTP API and raw records say **slot**
(`/v1/slots`, `"slot": "s104"`), **checkpoint** (`/v1/checkpoints`), **branch** (`POST /v1/slots/{id}/branch`) and
**capture** (`POST /v1/slots/{id}/capture`). They are the same things. An installed image (`osworld-ubuntu`) is a
checkpoint too, one that is never deleted.

## Not supported

More than one host, GPUs, images other than the pinned OSWorld Ubuntu image, network access from inside the desktop.
