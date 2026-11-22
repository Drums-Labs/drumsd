# Release notes: RC1 (2026-10-31)

RC1 was the first release candidate of the Drums runtime: drumsd, its Python runtime (then a separate repository, now
`python/` here), the drums-criu fork and the verifiers package, built and tested together as one release tarball. It
was an internal build and was not published.

What RC1 introduced, in short:

- Desktops run in the hardened slot profile by default (`--trusted-unhardened` / `"harden": false` opts out on
  trusted-only hosts).
- The per-env memory and CPU view (procview, BPF + FUSE) is opt-in (`--procview`).
- No privilege escalation inside a slot: the agent's processes run with `no_new_privs`.
- Gates for app crashes (`no_app_crash`) and crash taint, and a lab reward-function hook for custom tasks.
- drums-criu fixes for restores of a slot in its own user namespace and for memory protection keys across
  checkpoint and restore (a forked Chrome no longer crashes).

The commit-level detail of each release is in [CHANGELOG.md](../../CHANGELOG.md) and in `git log`.
