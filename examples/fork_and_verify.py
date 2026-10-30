"""End-to-end: one Chrome and one LibreOffice Calc OSWorld task; a few scripted steps; fork 8; a different scripted
continuation in each child; verify every child; close. The continuations are scripted policies (plumbing checks, not
agents): one does the task through a file oracle, the others do something else, so the rewards must differ by child.

    python examples/fork_and_verify.py --url http://127.0.0.1:7070 --token $DRUMS_TOKEN [--out run.json]

Prints one line per child (continuation, score, gate verdict, seconds) and the SDK metrics, and writes everything to --out.
"""
from __future__ import annotations

import argparse
import base64
import json
import os
import sys
import time
import urllib.request

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "python"))
import drums  # noqa: E402

CHROME = "2ad9387a-65d8-4e33-ad5b-7580065a27ca"     # "make a new folder on the bookmarks bar ... call it 'Favorites'"
CALC = "035f41ba-6653-43ab-aa63-c86d449d62e5"       # fill Gross profit, Sheet2 Year_Profit
CALC_GOLD = ("https://huggingface.co/datasets/xlangai/ubuntu_osworld_file_cache/resolve/main/libreoffice_calc/"
             "035f41ba-6653-43ab-aa63-c86d449d62e5/5_IncomeStatement2_gt1.xlsx")

# ---- Chrome continuations -------------------------------------------------------------------------------------------
BOOKMARKS_EDIT = r"""
import json, os, subprocess, time
subprocess.run(['pkill', '-x', 'chrome']); time.sleep(2)
p = os.path.expanduser('~/.config/google-chrome/Default/Bookmarks')
d = json.load(open(p)) if os.path.exists(p) else {"roots": {"bookmark_bar": {"children": [], "name": "Bookmarks bar", "type": "folder", "id": "1"},
     "other": {"children": [], "name": "Other bookmarks", "type": "folder", "id": "2"}, "synced": {"children": [], "name": "Mobile bookmarks", "type": "folder", "id": "3"}}, "version": 1}
d.pop('checksum', None)
d['roots'][ROOT]['children'].append({"children": [], "name": NAME, "type": "folder", "id": str(100 + len(d['roots'][ROOT]['children'])),
                                      "date_added": "13370000000000000", "date_modified": "0"})
json.dump(d, open(p, 'w'), indent=3)
subprocess.Popen(['google-chrome', '--remote-debugging-port=1337'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)
time.sleep(3)
print('ok')
"""


def chrome_folder(name, root="bookmark_bar"):
    def run(env):
        code = BOOKMARKS_EDIT.replace("ROOT", repr(root)).replace("NAME", repr(name))
        env.step({"type": "execute", "command": ["python3", "-c", code], "shell": False})
    run.__name__ = f"shell: folder {name!r} in {root}"
    return run


def chrome_gui_new_tab_typing(env):
    env.step("pyautogui.hotkey('ctrl', 't'); time.sleep(1); pyautogui.write('Favorites', interval=0.03); time.sleep(0.5)")
    env.step("pyautogui.press('escape'); time.sleep(0.5)")
chrome_gui_new_tab_typing.__name__ = "gui: new tab, type 'Favorites' in the omnibox, escape"


def chrome_gui_toggle_bar(env):
    env.step("pyautogui.hotkey('ctrl', 'shift', 'b'); time.sleep(1)")
chrome_gui_toggle_bar.__name__ = "gui: toggle the bookmarks bar"


def noop(env):
    env.step({"type": "sleep", "seconds": 1})
noop.__name__ = "nothing (sleep 1 s)"


CHROME_CONT = [chrome_folder("Favorites"), chrome_folder("Favourites"), chrome_folder("favorites"), chrome_folder("Favorites", "other"),
               chrome_gui_new_tab_typing, chrome_gui_toggle_bar, noop, chrome_folder("Favorites")]

# ---- Calc continuations ---------------------------------------------------------------------------------------------
_gold = {}


def gold_b64():
    if "b" not in _gold:
        with urllib.request.urlopen(CALC_GOLD, timeout=120) as r:
            _gold["b"] = base64.b64encode(r.read()).decode()
    return _gold["b"]


def calc_write_file(content_b64, label):
    code = ("import base64, subprocess, time\n"
            "subprocess.run(['pkill', '-f', 'soffice']); time.sleep(3)\n"
            f"open('/home/user/IncomeStatement2.xlsx', 'wb').write(base64.b64decode('{content_b64}'))\n"
            "subprocess.Popen(['libreoffice', '--calc', '/home/user/IncomeStatement2.xlsx'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, start_new_session=True)\n"
            "for i in range(90):\n"
            "    out = subprocess.run(['wmctrl', '-l'], capture_output=True, text=True).stdout\n"
            "    if 'IncomeStatement2.xlsx' in out: break\n"
            "    time.sleep(1)\n"
            "time.sleep(3)\nprint('ok')\n")

    def run(env):
        env.step({"type": "execute", "command": ["python3", "-c", code], "shell": False})
    run.__name__ = label
    return run


def calc_gold(env):
    calc_write_file(gold_b64(), "")(env)
calc_gold.__name__ = "file oracle: the task's expected workbook, reopened in Calc"


def calc_type(text):
    def run(env):
        env.step("pyautogui.hotkey('ctrl', 'home'); time.sleep(0.5)")
        env.step(f"pyautogui.write({text!r}, interval=0.02); pyautogui.press('enter'); time.sleep(0.5)")
    run.__name__ = f"gui: type {text!r} into A1"
    return run


def calc_new_sheet(env):
    env.step("pyautogui.hotkey('alt', 'shift', 'ctrl', 'pagedown'); time.sleep(1)")      # harmless key chord
    env.step("pyautogui.hotkey('ctrl', 'end'); pyautogui.write('1', interval=0.02); pyautogui.press('enter'); time.sleep(0.5)")
calc_new_sheet.__name__ = "gui: key chord, then type '1' after the last cell"


CALC_CONT = [calc_gold, calc_type("Year_Profit"), calc_type("=1+1"), calc_new_sheet, noop, calc_gold, calc_type("hello"), noop]


# ---- the run --------------------------------------------------------------------------------------------------------
def one_task(client, task_id, prefix, conts, log):
    t0 = time.monotonic()
    env = client.create(image="osworld-ubuntu", task=task_id)
    rec = {"task": task_id, "parent": env.id, "create_s": round(time.monotonic() - t0, 3), "create_timings": env.record.get("timings")}
    print(f"[{task_id[:8]}] parent {env.id} ready in {rec['create_s']} s (task setup {env.record.get('timings', {}).get('task_setup')} s)", flush=True)
    for a in prefix:
        r = env.step(a)
        log.append({"env": env.id, "action": a, "seconds": r.seconds})
    obs = env.observe()
    rec["parent_observation"] = {"size": [obs.width, obs.height], "meta": obs.meta}
    t1 = time.monotonic()
    kids = env.fork(8)
    rec["fork_s"] = round(time.monotonic() - t1, 3)
    print(f"[{task_id[:8]}] fork(8) in {rec['fork_s']} s: {[k.id for k in kids]}", flush=True)

    def cont(pair):
        i, (k, fn) = pair
        k.wait_valid()                   # the child's own gates (alive after resume) resolve before it acts
        ts = time.monotonic()
        fn(k)
        cs = time.monotonic() - ts
        rw = k.verify(strict=False)      # strict=False: record every reward with its verdict (never trained on unless valid)
        return {"child": k.id, "i": i, "continuation": fn.__name__, "continuation_s": round(cs, 3), "score": rw.score,
                "valid": rw.valid, "verdict": rw.verdict, "bad_gates": rw.bad_gates, "checker_error": rw.checker_error,
                "verify_s": rw.seconds}
    res = drums.Client.map(cont, list(enumerate(zip(kids, conts))), workers=8)
    rec["children"] = [r if isinstance(r, dict) else {"error": f"{type(r).__name__}: {r}"} for r in res]
    for r in rec["children"]:
        if "error" in r:
            print(f"[{task_id[:8]}]   ERROR {r['error']}", flush=True)
        else:
            print(f"[{task_id[:8]}]   {r['child']:>6} score={r['score']} valid={r['valid']} verify={r['verify_s']}s  {r['continuation']}", flush=True)
    rp = env.verify(strict=False)
    rec["parent_reward"] = {"score": rp.score, "valid": rp.valid, "verdict": rp.verdict}
    rec["child_metrics"] = {k.id: k.metrics() for k in kids}
    for k in kids:
        k.close()
    rec["parent_metrics"] = env.metrics()
    env.close()
    rec["total_s"] = round(time.monotonic() - t0, 3)
    return rec


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default=os.environ.get("DRUMS_URL", "http://127.0.0.1:7070"))
    ap.add_argument("--token", default=os.environ.get("DRUMS_TOKEN"))
    ap.add_argument("--out", default="fork_and_verify.json")
    a = ap.parse_args()
    client = drums.connect(a.url, a.token)
    h = client.health()
    print(f"drumsd {h['version']} criu {h['criu']['sha256'][:12]} images {[c['id'] for c in h['checkpoints']]}", flush=True)
    m0 = client.reset_metrics()
    log = []
    out = {"health": h, "previous_window": m0, "tasks": []}
    chrome_prefix = ["time.sleep(2); pyautogui.moveTo(960, 540)", "pyautogui.hotkey('ctrl', 't'); time.sleep(1)", "pyautogui.hotkey('ctrl', 'w'); time.sleep(1)"]
    calc_prefix = ["time.sleep(2); pyautogui.moveTo(960, 540)", "pyautogui.hotkey('ctrl', 'home'); time.sleep(0.5)", "pyautogui.press('down'); time.sleep(0.5)"]
    for tid, prefix, conts in ((CHROME, chrome_prefix, CHROME_CONT), (CALC, calc_prefix, CALC_CONT)):
        try:
            out["tasks"].append(one_task(client, tid, prefix, conts, log))
        except drums.DrumsError as e:
            print(f"[{tid[:8]}] FAILED: {type(e).__name__}: {e}", flush=True)
            out["tasks"].append({"task": tid, "error": f"{type(e).__name__}: {e}", "record": getattr(e, "record", None)})
    out["steps"] = log
    m = client.metrics()
    out["metrics"] = m
    u, c = m["usage"], m["cost"]
    print("\nmetrics (this run's window):")
    for k, v in sorted(u["ops"].items()):
        print(f"  {k:<11} n={v['n']:<4} failed={v['failed']:<3} p50={v['p50_s']}s p95={v['p95_s']}s total={v['seconds_total']}s")
    print(f"  episodes {u['episodes']}  failures {u['failures_by_cause']}  retries {u['retries_by_cause']}")
    print(f"  rewards {u['verify']}")
    print(f"  cost: {c['host_usd']} USD at {c['host_usd_per_hour']} USD/h over {c['wall_hours']} h; "
          f"{c['usd_per_valid_episode']} USD per valid episode; {c['usd_per_verified_reward']} USD per verified reward")
    json.dump(out, open(a.out, "w"), indent=1, default=str)
    print(f"wrote {a.out}")


if __name__ == "__main__":
    main()
