# Install and first run

Drums runs isolated Linux desktops (the OSWorld Ubuntu image) as checkpointed process trees on one host. You create
environments, act in them, fork a reached state into copies, and get rewards from OSWorld's checkers, each tagged with
whether every isolation and correctness gate passed. Names: the SDK's env, saved state, fork and save are the API's
slot, checkpoint, branch and capture. This page is the long form of the README's quickstart: the same install, with
more about the host, the options and what to know afterwards.

## 1. Host

A fresh Ubuntu 24.04 x86_64 machine with cgroup v2 (the default), 16 vCPU, 32 GiB RAM and a 150 GiB disk, with outbound
HTTPS during the install. Tested: AWS c8i.4xlarge in us-east-1 with the latest Ubuntu 24.04 AMI. Details, the network
list and how many envs fit: [host-requirements.md](host-requirements.md).

## 2. Install

A release is one tarball, `drums-release-<commit>.tar.gz`, from the
[releases page](https://github.com/Drums-Labs/drumsd/releases) (or built from a tagged checkout with
`install/make-release.sh`, see [CONTRIBUTING.md](../CONTRIBUTING.md#releases)).

Download four files from the same release: the tarball, its `.sha256`, its `.sigstore.json` (the signature) and
`verify-release.sh`. Check them before you unpack anything. The signature is Sigstore keyless: it proves the tarball was
built by this repository's `.github/workflows/release.yml` on a `v*` tag, with no key for anyone to lose. It needs
[cosign](https://docs.sigstore.dev/cosign/system_config/installation/) 2.x.

```bash
V=vX.Y.Z   # the release you want
gh release download "$V" -R Drums-Labs/drumsd   # or download the four files from the releases page
bash verify-release.sh --require-signature drums-release-*.tar.gz
```

> **v0.2.2 is not signed.** It was built outside the release workflow, so it has no `.sigstore.json`. For v0.2.2, run
> `bash verify-release.sh drums-release-*.tar.gz` without `--require-signature`: it checks the sha256 and warns that no
> signature was checked. The sha256 is in the release notes. Signed releases resume with the next tag.

Expected output (the hash is the release's own):

```
[verify-release] sha256 ok: <64 hex characters>
[verify-release] signature ok: signed by Drums-Labs/drumsd .github/workflows/release.yml on a v* tag (https://token.actions.githubusercontent.com)
```

Anything else ends in `[verify-release] REFUSED: ...` and exit status 1: do not install that tarball. Without cosign,
leave out `--require-signature`: the checksum is still checked, and a warning says the signature was not. The
same check by hand:

```bash
sha256sum -c drums-release-*.tar.gz.sha256
cosign verify-blob --bundle drums-release-*.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/Drums-Labs/drumsd/\.github/workflows/release\.yml@refs/tags/v[0-9][^/]*$' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com drums-release-*.tar.gz
```

Then install:

```bash
tar xzf drums-release-*.tar.gz
cd drums-release-*/
sudo ./install.sh
```

`install.sh` checks every file of the release against its `SHA256SUMS`, and runs `install/verify-release.sh` again
on the tarball when it is still next to the unpacked directory; `sudo ./install.sh --require-signature` also refuses
unless the signature bundle verifies. Those checks run code that came in the same tarball, so they catch a corrupted
or incomplete download, not a tampered one: a tampered tarball can carry a tampered checker. Proof of origin is the
`verify-release.sh` you downloaded separately, run as above before you unpack. Releases before the signed release workflow (v0.2.1 and earlier) have
a `.sha256` but no `.sigstore.json`.

It takes 7 to 15 minutes and prints a progress line every 30 s.

At the end it prints the URL (`http://127.0.0.1:7070`) and where the API token is: `/etc/drums/token` (root only, 0600).
The token is never printed. Every input is pinned in `install/lock.json` and checked; the first mismatch stops the
install. Log: `/var/log/drums-install.log`. Running `install.sh` again is safe (it reuses what is already there).

The API listens on 127.0.0.1. From your laptop, tunnel it:

```bash
ssh -L 7070:127.0.0.1:7070 YOUR_HOST
```

`--remote` binds all interfaces instead, over plain HTTP with only the bearer token: use it only inside a private
network. Every option:

| flag | effect |
|---|---|
| `--remote` | bind every interface instead of 127.0.0.1 |
| `--port 7070` | API port |
| `--host-usd-per-hour X` | the host's price, for the cost metrics (default: looked up for the EC2 instance type, else 0) |
| `--state-tmpfs-gib N` | size of the RAM-backed state directory (default: half of RAM) |
| `--swap-mib N` | swap per env (needs host swap, for example zram; default 0) |
| `--procview` | the opt-in per-env memory and CPU view ([procview.md](procview.md)) |
| `--no-procview` | the default; turns the view off again on a host that had it |
| `--trusted-unhardened` | hardened profile off: trusted code only ([security.md](security.md)) |
| `--require-signature` | also check the Sigstore bundle of the tarball next to the release directory (detects corruption; proof of origin is the separately downloaded `verify-release.sh`, run first) |

Envs run in the **hardened profile** by default. Each desktop gets its own user namespace, so root in it is an
unprivileged host uid. It also gets no_new_privs, a capability bounding set, seccomp and a metadata firewall. The desktop
user has no sudo. See [security.md](security.md). `--trusted-unhardened` turns the profile off, and only belongs
on hosts that run code you trust. `--procview` (opt-in) gives each env its own memory/CPU view: [procview.md](procview.md).

## 3. Connect

The SDK is standard-library Python (3.9 or later), published as `drums-sdk`; the release also ships the same wheel
under `/opt/drums/sdk/` for hosts without PyPI access.

```bash
python3 -m venv ~/drums-venv
~/drums-venv/bin/pip install drums-sdk
```

On the host:

```bash
export DRUMS_URL=http://127.0.0.1:7070
export DRUMS_TOKEN=$(sudo cat /etc/drums/token)
```

From your laptop, through the tunnel above:

```bash
export DRUMS_URL=http://127.0.0.1:7070
export DRUMS_TOKEN=$(ssh YOUR_HOST sudo cat /etc/drums/token)
```

```python
import drums
with drums.connect() as client:                         # DRUMS_URL, DRUMS_TOKEN; closes this client's envs at the end
    client.reset_metrics()                              # cost from now on (the window otherwise includes idle time)
    with client.create(task="e0df059f") as env:         # an OSWorld task id (8+ characters), a JSON path, or a dict
        env.wait_valid()                                # this episode's gates passed (5 s after ready)
        obs = env.observe()                             # obs.png (1920x1080 PNG), obs.meta (instruction, ...)
        env.step("pyautogui.hotkey('ctrl', 'alt', 't')")   # pyautogui code, or an action dict
        with env.fork(4, keep_state=False) as kids:      # 4 independent copies; their saved state is deleted with them
            for k in kids:
                k.step({"type": "execute", "command": ["ls", "Desktop"]})
                r = k.verify(strict=False)              # the task's checker
                print(k.id, r.score, r.valid, r.invalid_reasons)
        state = env.save()                              # later: client.create(from_state=state)
        client.delete_state(state)                      # or let its lease delete it (30 min unused)
    print(client.metrics()["cost"])                     # USD per valid episode / verified reward, this window
```

A longer loop: [examples/rl_loop.py](../examples/rl_loop.py). All actions and what a screenshot is:
[actions-and-observations.md](actions-and-observations.md).

## 4. What to know

- **Train only on `Reward.valid`.** `verify()` raises `InvalidEpisode` instead of returning a reward from an invalid
  episode; `verify(strict=False)` returns it with `valid=False` and `invalid_reasons`. A quarantined env is always
  invalid. [validity.md](validity.md).
- **The first 5 s of an episode.** create, reset and fork children are `ready` with verdict `pending`; the last gate
  resolves 5 s later. An action that ends a process in those 5 s makes the episode invalid: call `env.wait_valid()` first.
- **Cleanup.** Envs and saved states unused for 30 min are reaped by drumsd (leases; `idle_timeout=`/`ttl=` on create,
  fork, save; 0 = never). The SDK also closes the envs a client created at exit, on SIGTERM/SIGHUP and at the end of a
  `with` block. `fork()` returns the children plus `.state`; `keep_state=False` deletes it with them.
  [lifecycle.md](lifecycle.md).
- **Errors are typed.** `RestoreFailed` (env down; `reset()` retries), `GateFailed` (env quarantined), `TaskSetupFailed`,
  `ActionFailed` (the action ran and exited non-zero; `step(..., check=False)` returns it), `EnvGone` (the env was closed
  or reaped: replace it), `Overloaded` (the host is full: retry later), `NotQuiescent` (fork/save found no stable
  moment in 30 s; nothing was saved), `CheckpointStale` (re-run the installer). drumsd retries only failures before any
  restored process ran; the SDK retries only GETs while drumsd restarts.
- **fork() and save() are safe right after an action.** drumsd waits until the process tree is quiescent and checks the
  dump against it.
- **Metrics.** `client.metrics()` and `env.metrics()`: latency per operation, episodes by verdict, failures and retries by
  cause, rewards, and a cost from the host price per hour set at install (`--host-usd-per-hour`).
- **Reboots.** `drums-boot.service` and `drums-origin.service` bring the host back after a stop/start (a few minutes);
  envs that were running are gone.
- **API.** [api/openapi.json](../api/openapi.json) (also `GET /openapi.json`). Every route except `GET /healthz` needs
  `Authorization: Bearer <token>`; the README lists every route.
