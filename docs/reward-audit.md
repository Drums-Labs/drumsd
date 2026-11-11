# Reward audit

Every `POST /v1/slots/{id}/verify` reply (the SDK's `Reward`) carries an `audit` block that says how far this one
reward can be trusted. It is measured on the rollout, never assumed, and it never changes `score` or `valid`: a
trainer decides what to do with a flagged reward (drop it, down-weight it, or log it).

```python
r = env.verify()
r.score, r.valid          # as before
r.trusted                 # True only for a valid episode with a score and no audit flag (None on an older drumsd)
r.audit_flags             # e.g. ["scope_violation"]
r.audit["recheck"]        # {"ran": True, "score": 1.0, "agree": True, "seconds": 0.05}
```

| check | what runs | flag |
|---|---|---|
| checker | the first call's seconds and error class (`none`, `exception`, `timeout`, `bad_return`, `helper_failed`) | `checker_crash`, `checker_timeout` |
| recheck | the same checker a second time on the same state. Lab verifiers always; OSWorld evaluators only when they have no `postconfig` (whose actions could change the state) | `nondeterministic` |
| redundant | an independent checker the task declares: `evaluator.redundant = {"verifier", "function", "options"}` (options default to the primary's) | `redundant_disagree` |
| scope | the files under `evaluator.scope.roots` (scanned in the desktop as the desktop user, sha256 each) compared with `scope.manifest` (`{absolute path: sha256}` of the start state); a change, deletion or new file that no `scope.allow` glob covers | `scope_violation` (or `scope_unchecked` if the scan failed) |
| episode | the gate verdict and quarantine state | `invalid_episode`, `checker_skipped` |

- Each checker call has a deadline (`evaluator.deadline_s`, default 90 s; drumsd's own `osw.eval_timeout_s` still
  bounds the whole call). Over the deadline is a checker error of class `timeout`, never a score.
- The scope scan is a reward-hacking **signal**, not a security boundary: it runs with the desktop user's rights and
  ignores LibreOffice lock files and `__pycache__`.
- Metrics: `drumsd_audit_total{trusted}`, `drumsd_audit_flag_total{flag}`.
