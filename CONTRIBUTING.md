# Contributing to drumsd

drumsd is the daemon that owns Drums desktop slots on a Linux host. It drives the Drums fork of CRIU
([drums-criu](https://github.com/Drums-Labs/drums-criu)) and is talked to by the Python SDK
([drums-python-sdk](https://github.com/Drums-Labs/drums-python-sdk), `pip install drums-sdk`). This file says how to
build it, which tests gate a change, how a release is cut, and what a pull request needs.

## Build

Rust (the toolchain `install/lock.json` names under `rust_toolchain`), Linux. macOS builds the crates but cannot run
the daemon or most tests: slots need cgroups v2, user namespaces, overlayfs and CRIU.

```bash
cargo build --release
```

The workspace has five crates: `drumsd-core` (records, gates, journal), `drumsd-linux` (cgroups, namespaces, mounts,
crash watch), `drumsd` (the daemon, HTTP API, manager), `drums-pool` (multi-host pool) and `drumsd-os` (Android and
Windows pools). Config fields are in `crates/drumsd/src/config.rs`; the HTTP surface is `api/openapi.json`.

## Tests

### Unit tests (every change)

```bash
sudo cargo test --release
```

Run them as root: the netns forwarder and nested-pidns tests create namespaces and fail with EPERM otherwise.
`cargo test` without root runs everything else (the four tests that need root skip themselves and say so; CI runs it
this way, with `zstd` installed for the `forget` tests), but the PR runs the full set as root.

Also, when you touch them:

```bash
python3 -m unittest install/tests/test_build_wheel.py      # the SDK wheel builder (install/build_wheel.py)
python3 -m unittest discover -s osw/tests                  # the OSWorld task bridge and its verifier
for t in install/tests/test_*.sh; do bash "$t"; done       # install.sh output, boot, retry
```

### Host tests (anything that touches restore, gates, lifecycle or the installer)

These run on an installed Linux host, as root, against the real daemon, with the SDK from the release's wheel:

```bash
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

1. Update `install/lock.json`: the `drums-criu` and `drums-sdk` commits (branch, commit, a `note` that says
   what the commit is), the apt snapshot and package versions, the OSWorld image and its sha256, the Rust toolchain.
   The installer verifies every one of these; a wrong hash fails the install, it does not warn.
2. Add the release to `CHANGELOG.md` (what changed, correctness first, then security, then performance, then pins).
   `docs/README.md` becomes the release's README; keep it current.
3. Commit. `make-release.sh` refuses a tree with uncommitted changes.
4. On a machine with the four repositories checked out at (or containing) the pinned commits:

   ```
   DRUMS_CRIU=path/to/drums-criu DRUMS_SDK=path/to/drums-python-sdk \
       install/make-release.sh OUT_DIR
   ```

   It writes `OUT_DIR/drums-release-<commit12>/` and `drums-release-<commit12>.tar.gz`: `install.sh`, `install/`,
   `LICENSE`, `NOTICE`, `THIRD_PARTY_LICENSES`, git bundles of drumsd and drums-criu at the pinned commits (and no
   other bundle), `python/`, the SDK wheel built from the pinned SDK commit (`install/build_wheel.py`, standard
   library only), the user docs (`README.md`, `docs/install.md`, `CHANGELOG.md`, `docs/`, `examples/`,
   `api/openapi.json`), `RELEASE_COMMIT` and `SHA256SUMS`. Then `install/check-release.py` checks the tarball: an
   allow list of entries, exactly those two bundles (drumsd's must be this repository's history, drums-criu's must
   contain upstream v4.3), the licence files, `SHA256SUMS`, and no keys, internal paths, internal host names or
   account ids in any file, in the wheel, or in the bundles' history. A refused tarball is deleted. Put private
   patterns of your own in a file outside the repository and point `DRUMS_RELEASE_DENYLIST` at it. On success it
   prints the tarball's sha256.
5. Install it on a fresh host of the type `docs/host-requirements.md` names (`sudo ./install.sh`), then run the host
   tests above against it. The release is good when `integration_host.py` and `recovery_host.py` exit 0.
6. Tag the drumsd commit (`vX.Y.Z`) and tag the pinned drums-criu and SDK commits with the same version. Pushing the
   drumsd tag runs `.github/workflows/release.yml`: it builds the tarball again with `make-release.sh` on a GitHub
   runner from the pinned commits, writes `drums-release-<commit12>.tar.gz.sha256`, signs the tarball with Sigstore
   keyless (`cosign sign-blob --bundle`, the job's GitHub OIDC identity; no signing key exists), checks the signature
   with `install/verify-release.sh --require-signature`, and creates a **draft** release with the tarball, the
   `.sha256`, the `.sigstore.json` and `verify-release.sh`. The signature is a public Rekor entry naming the repository,
   the workflow and the tag.

   The workflow checks out drums-criu and drums-python-sdk at the pinned commits. Public repositories need no
   token. While either is private, the tag build needs a repository secret `RELEASE_SOURCES_TOKEN`: a fine-grained
   personal access token, resource owner Drums-Labs, repository access limited to `drums-criu` and
   `drums-python-sdk`, permission **Contents: Read-only** and nothing else, with a short expiry
   (`gh secret set RELEASE_SOURCES_TOKEN -R Drums-Labs/drumsd`). Only the tag build reads it; pull requests use the
   default token, so on a pull request the build part works only once both repositories are public.
7. Download the draft's four files and check them the way a user does (`docs/install.md` section 2):

   ```
   bash verify-release.sh --require-signature drums-release-<commit12>.tar.gz
   [verify-release] sha256 ok: <64 hex characters>
   [verify-release] signature ok: signed by Drums-Labs/drumsd .github/workflows/release.yml on a v* tag (https://token.actions.githubusercontent.com)
   ```

   Then publish the draft. A pull request that touches `install/` or the release workflow runs the build part of the
   same workflow (no signing, no release), so a broken `make-release.sh` shows up before the tag.

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
- `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets -- -D warnings` clean (`rustfmt.toml` sets
  the width). CI (`.github/workflows/ci.yml`) runs both, the non-root `cargo test --workspace`, shellcheck over
  `install/*.sh` and the Python tests on every push and pull request; the root-only and host tests are the PR
  author's, who says which ones ran in the PR.
