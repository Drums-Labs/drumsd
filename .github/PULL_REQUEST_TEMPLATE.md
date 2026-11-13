## What changed and why

<!-- Two or three sentences. One change per PR. Link the issue if there is one. -->

## How it was tested

<!-- CI runs fmt, clippy, the non-root unit tests, shellcheck. Say what else you ran. -->

- [ ] `cargo test --workspace` on Linux (and `sudo -E cargo test --workspace` if the change touches namespaces, cgroups or uid ranges)
- [ ] Host tests, if the change touches restore, gates, lifecycle or the installer (`tests/host/integration_host.py`, `recovery_host.py`; output attached)
- [ ] `python3 -m unittest discover -s osw/tests` / `install/tests`, if touched

## Checklist

- [ ] Gates still fail closed; a change that turns an uncertain episode valid says why in `docs/validity.md`
- [ ] `api/openapi.json`, the SDK and `docs/` are in step with any route or field change
- [ ] `CHANGELOG.md` has an entry under the unreleased version
- [ ] No secrets, tokens, hostnames, instance ids or customer names in code, tests, fixtures, docs or logs
- [ ] Commits are signed off (`git commit -s`, DCO)
