"""Checker engine, identical in rules to crates/drumsd-os/src/tasks.rs::judge (used to score incumbent arms)."""
import json, os, re


def judge(c, stdout, rc):
    s = stdout.strip()
    short = s[:200]
    keys = ("equals", "regex", "contains", "not_contains", "min", "max")
    if rc != 0 and not any(c.get(k) is not None for k in keys):
        return False, f"exit {rc}"
    if c.get("equals") is not None and s != c["equals"].strip():
        return False, f"got {short!r}, want {c['equals']!r}"
    if c.get("regex") is not None and not re.search(c["regex"], s, re.M):
        return False, f"{short!r} does not match /{c['regex']}/"
    if c.get("contains") is not None and c["contains"] not in s:
        return False, f"{short!r} lacks {c['contains']!r}"
    if c.get("not_contains") is not None and c["not_contains"] in s:
        return False, f"{short!r} contains {c['not_contains']!r}"
    if c.get("min") is not None or c.get("max") is not None:
        try:
            v = float(s)
        except ValueError:
            return False, f"{short!r} is not a number"
        if (c.get("min") is not None and v < c["min"]) or (c.get("max") is not None and v > c["max"]):
            return False, f"{v} outside [{c.get('min')}, {c.get('max')}]"
    return True, f"ok: {short!r}"


def load_tasks(d):
    out = {}
    for f in sorted(os.listdir(d)):
        if f.endswith(".json"):
            v = json.load(open(os.path.join(d, f)))
            for t in v if isinstance(v, list) else [v]:
                out[t["id"]] = t
    return out


if __name__ == "__main__":
    # self-test mirroring tasks.rs::judges
    assert judge({"equals": "255"}, "255\n", 0)[0]
    assert not judge({"equals": "255"}, "254", 0)[0]
    assert judge({"regex": "^name=Ada$"}, "x\nname=Ada\ny", 0)[0]
    assert not judge({"contains": "a"}, "b", 0)[0]
    assert not judge({"not_contains": "a"}, "a", 0)[0]
    assert judge({"min": 1.3}, "1.5", 0)[0]
    assert not judge({"min": 1.3}, "1.0", 0)[0]
    assert not judge({"min": 1.3}, "abc", 0)[0]
    assert judge({}, "", 0)[0] and not judge({}, "", 1)[0]
    print("checker self-test ok")
