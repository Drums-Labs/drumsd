# Design: the sandboxed verifier runtime

Status: implemented in drumsd 0.5 (store, upload API, confined execution, verdict hash, budgets, bundled `drums_t6`).
Audit: A-13 (no way for a client to add a checker; the release ships none), A-15 (check kinds), A-26 (crash detail).
Security posture: design note 2026-11-16 sections 5.2 and 5.3.

## The problem

A factory task names a lab verifier (`"func": "drums_verifier"`). Today that is a Python file the operator installs as
root in `/etc/drums/verifiers`, run by the OSWorld helper on the host as the `drums-osw` user. The release ships no
verifier, so a fresh host cannot score the factory's tasks, and a client cannot add a checker at all: the only way in
is root on the host. We want a client to upload their own checker and have it run confined, never as a host process
with the helper's rights and never as root.

## Threat model

- **The checker is client code.** It may be careless (an infinite loop, a 10 GB allocation, a fork in a loop) or
  hostile (read the host's API token, other tenants' files, the cloud metadata service; write to the host; escalate;
  phone home). It must not be able to do any of that, and a broken checker must cost at most its own budget.
- **The agent is untrusted.** It acted in the desktop before the checker runs and may have planted anything there:
  files shaped to crash a parser, processes left running, a rewritten copy of anything the desktop user owns. The
  checker must not run where the agent's processes can reach it, and its code must not be writable from the desktop.
- **The task JSON is untrusted** (any API client sends it). It may name a verifier, a function and options; it never
  carries code. Code arrives only through the upload API, which is behind the host token (and, on Drums Cloud, behind
  the gateway's workspace auth).
- Out of scope: kernel exploits (the sandbox is namespaces + seccomp + cgroups, not a VM; see "What this is not").

## Decisions

### 1. Where it runs: a sibling sandbox on the host, not inside the desktop

drumsd (root) starts each checker call as a fresh process, `drumsd verifier-sandbox`, which confines itself and then
execs the checker venv's Python on a small standard-library runner. It is a sibling of the desktop, not a process in
it. Reasons:

- The desktop is the agent's territory. A checker run as the desktop user inside the slot shares a uid, a pid
  namespace and a home directory with whatever the agent left running; the agent could ptrace it, replace its files or
  race its reads. compare/c1's in-desktop runner was fine for a comparison harness; it is not a security boundary.
- The checker needs the checker venv (openpyxl, python-docx, PyMuPDF, ...). The desktop image does not have it, and
  putting it there would let the agent alter it.
- One mechanism for every image: the sandbox does not depend on what the desktop image contains.

The sandbox gets the same classes of confinement a slot has, tighter:

| layer | setting |
|---|---|
| user | a dedicated unprivileged uid/gid (`drums-verify`, created by the installer; never root, never the desktop user, never `drums-osw`), no supplementary groups |
| privileges | `no_new_privs`; no capabilities after the uid change; setuid binaries do nothing |
| mount namespace | a new root on a tmpfs, `pivot_root`ed, old root detached: only an allow list is visible, read-only (`/usr`, `/lib*`, `/bin`, the checker venv, `/etc/ld.so.cache`, `/etc/localtime`), plus a size-capped writable `/tmp`, a fresh `/proc` and `/dev/{null,zero,full,random,urandom}`. `/etc/drums` (the token), `/var/lib/drums`, `/home`, `/root` and `/run` do not exist in it |
| network namespace | a new, empty one: no interface is up, not even loopback. No network, no metadata service, no drumsd API |
| pid, ipc, uts, cgroup namespaces | new: the checker sees only its own processes and cannot signal anything outside |
| seccomp | deny (EPERM) mount, pivot_root, chroot, unshare, setns, ptrace, bpf, perf_event_open, keyctl, io_uring, userfaultfd, kexec, module loading, process_vm_*, open_by_handle_at and the new mount API; `clone` with any namespace flag; `socket` for every family except AF_UNIX; `clone3` answers ENOSYS (libc falls back to `clone`). A foreign architecture or x32 call kills the process |
| cgroup | one cgroup per call with `memory.max`, `memory.swap.max = 0`, `pids.max` and `cpu.max`; the wall budget ends with `cgroup.kill` |
| rlimits | RLIMIT_CPU, RLIMIT_FSIZE, RLIMIT_NOFILE, RLIMIT_CORE = 0 as a second line |
| environment | cleared (`PATH`, `HOME=/tmp`, `PYTHONDONTWRITEBYTECODE`), `python -I -B`, every inherited fd above 2 closed |

### 2. How the checker sees the desktop: a broker on a pipe

The checker has no network and no view of the slot's files. Its `ctx` talks to drumsd over its own stdin/stdout, one
JSON line per request; drumsd answers from the slot's guest server:

- `ctx.read_file(path)` -> the bytes or `None`. Total bytes read are capped (the read budget).
- `ctx.run(command, shell=False, timeout=None)` -> `{"returncode", "output", "error"}`, run in the desktop as the
  desktop user through the guest server, with its timeout capped by what is left of the wall budget.
- `ctx.options`, `ctx.task`, `ctx.workdir` (`/tmp/work`, private to this call).

This is the same `ctx` the host-side helper gives a named verifier today, so a checker's code does not change. The
desktop's files are therefore read-only to the checker by construction (it gets bytes, not a mount); the only way it
can change the desktop is `ctx.run`, with exactly the desktop user's rights, after the agent's turn is over. The
checker's own `print` goes to its stderr (kept in the drumsd log, capped), never into the protocol.

### 3. Budgets

Defaults (drumsd.json `verifiers.budget`, every field optional):

| budget | default | what happens at the limit |
|---|---|---|
| wall | 90 s (the task's `evaluator.deadline_s` may lower it, or raise it up to `max_wall_s`, 600 s) | `cgroup.kill`; checker error class `timeout` |
| CPU | 60 CPU-seconds, at most 1 CPU at a time (`cpu.max`) | SIGXCPU/SIGKILL; class `cpu` |
| memory | 1024 MiB, no swap | the cgroup's OOM killer; class `oom` |
| processes | 64 | `fork` fails with EAGAIN; a checker that loops on it ends at the wall budget |
| writable `/tmp` | 256 MiB | ENOSPC inside the checker |
| bytes read from the desktop | 256 MiB per call | `read_file` raises |
| broker requests | 10,000 per call | the request raises |
| verdict size | 1 MiB | class `protocol` |

A checker error is never a score: it comes back as `checker_error` with its class, as today.

### 4. Output: one JSON verdict

The runner writes exactly one final line `{"op": "verdict", "score": ..., "info": ..., "error": ..., "error_class":
...}`. drumsd checks it (a finite number, `info` JSON, within the size cap), adds what it measured (seconds,
CPU-seconds, peak memory, OOM kills, requests, bytes read) and the verifier's sha256. No verdict line, a malformed one
or a second one is class `protocol`. The score rules are the helper's (`lab_score`): a number or a bool, or
`{"score": number, "info": {...}}`.

### 5. The verifier store: content-addressed, uploaded through the API

- `POST /v1/verifiers` (body: the Python source; `?name=` optional, for display) stores it under its sha256 in
  `verifiers.store_dir` (default `/var/lib/drums/verifiers`, root-owned, 0755; files 0644, written atomically). It
  answers `{"sha256", "bytes", "name", "created"}`; uploading the same bytes again is a no-op. Limits: 1 MiB
  (`verifiers.max_bytes`), UTF-8, no NUL bytes. `GET /v1/verifiers`, `GET /v1/verifiers/{sha256}`, `DELETE
  /v1/verifiers/{sha256}`.
- Every read re-hashes the file; a file whose bytes no longer match its name is refused (tamper or disk error).
- A task names an uploaded verifier by hash: `{"func": "drums_verifier", "verifier_sha256": "<64 hex>", "function":
  "check", "options": {...}}` (`verifier` stays as an optional display name). A redundant checker may name its own
  `verifier_sha256`, default the primary's.
- Creating an env with, or loading, a task whose `verifier_sha256` is not in the store fails at once with 422
  `verifier_missing` naming the hash, so a client uploads and retries before any episode runs.
- **Scoping.** drumsd has one token and one tenant per host (design note 5.4: one workspace per host until per-sibling
  uid remapping ships). Per-workspace scoping belongs to the gateway: it will proxy the upload, record which workspace
  owns which hash, and refuse a task naming a hash its workspace did not upload. drumsd keeps an optional owner label
  in each record so the gateway has somewhere to put it.

### 6. The hash is recorded everywhere the score is

- The factory's task index (`index.json`) records `verifier_sha256`, and every generated task's evaluator names it.
- Every verdict carries it: `checker.verifier_sha256` in `POST /v1/slots/{id}/verify`, and
  `audit.redundant.verifier_sha256` for the redundant checker. Named verifiers (the `/etc/drums/verifiers` path) report
  the sha256 of the file that ran, too, so every reward can be traced to the exact bytes that scored it.

### 7. How the existing verify path maps onto it

The OSWorld helper stays the orchestrator: it runs `postconfig`, calls the checker, then the reward audit (recheck,
redundant checker, scope scan). Only the call itself moves. For a sandboxed verifier the helper sends `{sha256,
function, options, deadline_s}` to drumsd through the call's one-time capability URL (the same mechanism provisioning
steps use, `/provision/<token>/verifier`); drumsd runs the sandbox against that slot's guest server and returns the
verdict. So:

- the **first call**, the **recheck** and the **redundant checker** are each one sandbox run, with a fresh budget;
- the **scope scan** is unchanged (it runs in the desktop as the desktop user and is a signal, not a boundary);
- **OSWorld's own evaluators** are unchanged (pinned OSWorld code, not client code);
- a timeout, a crash or a bad return keeps its audit class (`checker_timeout`, `checker_crash`), and the new classes
  (`cpu`, `oom`, `protocol`, `sandbox`) count as `checker_crash`.

### 8. What changes for the factory's checkers

Nothing in their code: same `ctx`, same return value. What they can no longer do, and never should have: import a
package outside the checker venv, open a network connection, read host files, or write anywhere but `ctx.workdir`.
`drums_t6.py` does none of these (it was checked against the sandbox in the integration test). New check kinds (A-15:
italic, highlight) change its bytes and so its hash; that is the point: a new version is a new hash, and a task set
names the version it was validated with.

### 9. Shipping `drums_t6` (A-13)

The release carries `verifiers/drums_t6.py` from the drumsd commit, with its sha256 pinned in `install/lock.json`
(approved like CRIU's source pin). The installer checks the hash, installs it as `/etc/drums/verifiers/drums_t6.py`
(so D1-style tasks that name it work unchanged) and seeds the store with it (so tasks that name its hash work). A Rust
test fails if the file and the lock disagree.

### 10. Migration from `/etc/drums/verifiers`

1. Now: tasks that name a verifier by hash always run in the sandbox. Tasks that name one by file name keep today's
   path (host-side helper as `drums-osw`), unless `verifiers.sandbox_named: true`; either way the verdict now
   carries the file's hash.
2. After a soak with `sandbox_named: true` on a test host (same scores on D1's held-out set), the default flips: named
   verifiers are imported into the store on first use and run sandboxed.
3. Then the host-side path is removed and `/etc/drums/verifiers` becomes an operator's seed directory only.

## What this is not

- Not a VM boundary. A kernel bug reachable through the allowed syscalls breaks it. The seccomp list removes the
  usual escalation surfaces (namespaces, mounts, bpf, io_uring, keyrings, ptrace); a microVM is the next step if we
  ever run mutually hostile tenants on one host, which we do not claim today.
- Not multi-tenant isolation of the store. That is the gateway's job (section 5).
- Not a limit on what `ctx.run` can do in the desktop beyond the desktop user's rights; the checker runs after the
  episode, and the desktop is discarded at reset.
