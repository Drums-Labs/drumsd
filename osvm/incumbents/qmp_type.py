#!/usr/bin/env python3
"""Type a line into a VM through QMP send-key (used once to repair WindowsAgentArena's golden image; see
osvm/incumbents/waa-pywin32.md). Usage: qmp_type.py HOST PORT [--win-r] TEXT"""
import json, socket, sys, time
host, port = sys.argv[1], int(sys.argv[2])
args = sys.argv[3:]
winr = args and args[0] == "--win-r"
text = args[-1]
s = socket.create_connection((host, port)); f = s.makefile("rw")
def cmd(c, a=None):
    m = {"execute": c}
    if a: m["arguments"] = a
    f.write(json.dumps(m) + "\n"); f.flush()
    while True:
        r = json.loads(f.readline())
        if "return" in r or "error" in r:
            return r
f.readline(); cmd("qmp_capabilities")
SHIFT = {'"': "apostrophe", "_": "minus", ":": "semicolon", "(": "9", ")": "0", "|": "backslash", ">": "dot", "<": "comma", "!": "1", "@": "2", "#": "3", "$": "4", "%": "5", "&": "7", "*": "8", "+": "equal", "?": "slash", "{": "bracket_left", "}": "bracket_right"}
PLAIN = {" ": "spc", "-": "minus", "=": "equal", ".": "dot", ";": "semicolon", "'": "apostrophe", "\\": "backslash", "/": "slash", ",": "comma", "[": "bracket_left", "]": "bracket_right"}
def key(*names):
    cmd("send-key", {"keys": [{"type": "qcode", "data": n} for n in names]}); time.sleep(0.03)
if winr:
    key("meta_l", "r"); time.sleep(1.5)
for ch in text:
    if ch.isalpha() and ch.isupper(): key("shift", ch.lower())
    elif ch.isalnum(): key(ch)
    elif ch in SHIFT: key("shift", SHIFT[ch])
    elif ch in PLAIN: key(PLAIN[ch])
    else: raise SystemExit(f"no key for {ch!r}")
key("ret")
print("typed", len(text), "chars")
