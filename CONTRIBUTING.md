# Contributing to drumsd

drumsd is the daemon that owns Drums desktop slots on a Linux host. It drives the Drums fork of CRIU
([drums-criu](https://github.com/Drums-Labs/drums-criu)) and is talked to by the Python SDK
([drums-python-sdk](https://github.com/Drums-Labs/drums-python-sdk), `pip install drums-sdk`). This file says how to
build it, which tests gate a change, how a release is cut, and what a pull request needs.

## Build

Rust (the toolchain `install/lock.json` names under `rust_toolchain`), Linux. macOS builds the crates but cannot run
the daemon or most tests: slots need cgroups v2, user namespaces, overlayfs and CRIU.

```
cargo build --release
```

The workspace has five crates: `drumsd-core` (records, gates, journal), `drumsd-linux` (cgroups, namespaces, mounts,
crash watch), `drumsd` (the daemon, HTTP API, manager), `drums-pool` (multi-host pool) and `drumsd-os` (Android and
Windows pools). Config fields are in `crates/drumsd/src/config.rs`; the HTTP surface is `api/openapi.json`.

## Tests

### Unit tests (every change)

```
sudo cargo test --release
```

Run them as root: the netns forwarder and nested-pidns tests create namespaces and fail with EPERM otherwise.
`cargo test` without root is fine for a quick loop on crates that do not touch the kernel, but the PR runs the full
set as root.

Also, when you touch them:

```
python3 -m unittest install/tests/test_build_wheel.py      # the SDK wheel builder (install/build_wheel.py)
python3 -m unittest discover -s osw/tests                  # the OSWorld task bridge and its verifier
for t in install/tests/test_*.sh; do bash "$t"; done       # install.sh output, boot, retry
```

### Host tests (anything that touches restore, gates, lifecycle or the installer)

These run on an installed Linux host, as root, against the real daemon, with the SDK from the release's wheel:

```
sudo PYTHONPATH=$(ls /opt/drums/sdk/drums*.whl) python3 tests/host/integration_host.py OUT
sudo PYTHONPATH=$(ls /opt/drums/sdk/drums*.whl) python3 tests/host/recovery_host.py OUT
```

`integration_host.py` is the baseline: create, step, reset, capture, branch, verify, destroy, and every gate on each.
`recovery_host.py` kills drumsd idle and mid-reset, crashes a restore, kills the template server, and checks the
reconciliation. The others (`oom_host.py`, `forgotten_host.py`, `readiness_host.py`, `v042_host.py`) each cover one
area; run the one your change touches. Each prints JSON lines and exits with the number of failed checks. Put the
output files in the PR.

### The fork gate

A fork (`branch`) must give children that are `ready` and **valid**: every child passes every gate (`isolation`,
`state_hash`, `alive_after_resume`, `no_app_crash`, ...), a token written in one child is visible in no other, and a
child's reset returns it to the branch point. `integration_host.py` checks this (`branch_children_valid`,
`branch_isolation_gate`, `child_reset_returns_to_branch_point`); `readiness_host.py` stresses it with a Chrome env
forked right after a process starts exiting. A change that makes any child invalid is not mergeable, whatever the
reason. Restored applications that crash in children (Chrome under memory protection keys was the case that made this
a gate) are caught by `no_app_crash` and count as failures, not as flakes.

### The zdtm gate (CRIU pin bumps)

drumsd does not vendor CRIU; `install/lock.json` pins a drums-criu commit. Moving that pin needs, on the CRIU side:

1. `make -C test/zdtm` and `test/zdtm.py run -a` on the pinned commit, on the host OS `install/lock.json` names;
2. the Drums-specific tests in the `uns` flavor too (the root task in its own user namespace, as a hardened slot):
   `userns_remap01`, `userns_chrome01`, `userns_mntns01`, `pkey01`, the `pidns_*`/`userns_*` cases, and
   `test/others/userns-remap`, `test/others/pageshare-userns`, `test/others/pageshare-oom`;
3. a drums-criu fix comes with a zdtm test that fails before and passes after it.

Then the drumsd host tests above against the new build. Say in the PR which commit was tested and paste the zdtm
summary line.

## Releases

A release is a self-contained tarball that `install.sh` installs on a fresh host. It is cut from a clean checkout:

1. Update `install/lock.json`: the `drums-criu`, `drums-lab` and `drums-sdk` commits (branch, commit, a `note` that says
   what the commit is), the apt snapshot and package versions, the OSWorld image and its sha256, the Rust toolchain.
   The installer verifies every one of these; a wrong hash fails the install, it does not warn.
2. Add the release to `CHANGELOG.md` (what changed, correctness first, then security, then performance, then pins).
   `docs/README.md` becomes the release's README; keep it current.
3. Commit. `make-release.sh` refuses a tree with uncommitted changes.
4. On a machine with the four repositories checked out at (or containing) the pinned commits:

   ```
   DRUMS_CRIU=path/to/drums-criu DRUMS_LAB=path/to/drums-lab DRUMS_SDK=path/to/drums-python-sdk \
       install/make-release.sh OUT_DIR
   ```

   It writes `OUT_DIR/drums-release-<commit12>/` and `drums-release-<commit12>.tar.gz`: `install.sh`, `install/`,
   git bundles of drumsd, drums-criu and drums-lab at the pinned commits, the SDK wheel built from the pinned SDK commit
   (`install/build_wheel.py`, standard library only), the user docs (`README.md`, `QUICKSTART.md`, `CHANGELOG.md`,
   `docs/`, `examples/`, `api/openapi.json`), `RELEASE_COMMIT` and `SHA256SUMS`. It prints the tarball's sha256.
5. Install it on a fresh host of the type `docs/host-requirements.md` names (`sudo ./install.sh`), then run the host
   tests above against it. The release is good when `integration_host.py` and `recovery_host.py` exit 0.
6. Tag the drumsd commit (`vX.Y.Z`) and tag the pinned drums-criu and SDK commits with the same version.

## Pull requests

- One change per PR. A PR that changes restore, gates, the journal or the installer carries its host-test output.
- Gates fail closed. A change that turns an uncertain episode from invalid to valid needs to say why that is safe in
  the commit message and in `docs/validity.md`.
- Agents never escalate. Anything that needs root in an env runs in provisioning, by drumsd, never by the desktop
  user. See `docs/security.md`.
- Keep the SDK, `api/openapi.json` and `docs/` in step: a new route or field appears in all three.
- Plain commit messages: what changed and why, in sentences. No generated attribution lines.
- Sign off every commit (`git commit -s`, the Developer Certificate of Origin, https://developercertificate.org): by
  signing off you certify you have the right to submit the change under the Apache License 2.0 (LICENSE).
- No secrets, tokens, private hostnames, instance ids or customer names in code, tests, fixtures, docs or logs.
- `cargo fmt` and `cargo clippy --release` clean. There is no CI on this repository yet; the PR author runs the tests
  and says which ones in the PR.
