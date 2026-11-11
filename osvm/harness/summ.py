"""Quick look at a soak file: counts and every invalid or disagreeing episode (analyze.py makes the report tables)."""
import json, sys
eps = [json.loads(l) for l in open(sys.argv[1])]
eps = [e for e in eps if e.get("kind") == "episode"]
print(len(eps), "eps; valid", sum(e["valid"] for e in eps), "agree", sum(bool(e.get("agrees")) for e in eps))
for e in eps:
    if not e["valid"] or not e.get("agrees"):
        st = e.get("start") or {}
        print(e["task"], "oracle" if e["oracle"] else "noop", "valid", e["valid"], "r", e.get("reward"), (st.get("reason") or "")[:300],
              [c["detail"][:160] for c in (e.get("verify") or {}).get("checks") or [] if not c["pass"]][:2],
              [g for g in (e.get("verify") or {}).get("gates") or [] if g.get("ok") is not True][:2], e.get("error"),
              [s.get("error") for s in e.get("steps") or [] if not s.get("ok")][:2])
