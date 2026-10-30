# Actions and observations

## The screen

| | value |
|---|---|
| resolution | 1920 x 1080, 24-bit (Xorg dummy driver, display `:0`) |
| coordinates | pixels, origin (0, 0) at the top left, x to the right, y down |
| desktop | the OSWorld Ubuntu image's GNOME (X11) session, with the image's own login environment |

Models that answer in normalized coordinates (e.g. 0..1000) must be scaled to 1920 x 1080 by your code.

## Actions: `env.step(action)`

| action | form | what runs |
|---|---|---|
| pyautogui | `"pyautogui.click(100, 200)"` or `{"type": "pyautogui", "code": "..."}` | `python3 -c "<OSWorld's prefix><code>"` in the desktop. The prefix is OSWorld's: `import pyautogui; import time; import platform`, `pyautogui.FAILSAFE = False`, OSWorld's table of shifted characters. Any Python is allowed. |
| execute | `{"type": "execute", "command": ["ls", "-l"], "shell": false}` or `{"type": "execute", "command": "ls ~/Desktop", "shell": true}` | the command, through the image's OSWorld guest server; waits for it and returns its output |
| launch | `{"type": "launch", "command": ["gedit"], "shell": false}` | starts the command and returns at once (no output) |
| sleep | `{"type": "sleep", "seconds": 2}` | waits on the host (at most 60 s) |

The reply (`StepResult`) has `result` (`status`, `returncode`, `output`, `error` from the guest), `episode`, `validity`
(the episode's verdict at that moment) and `seconds`. `step(..., screenshot=True)` adds `observation`.

- A pyautogui or execute action that exits non-zero raises `ActionFailed` (with `returncode`, `output`, `error`).
  `step(..., check=False)` returns it instead (for commands whose exit status is the answer, e.g. `test -f`).
- A guest that does not answer within 130 s raises `GuestError`.
- A step on a quarantined env is `Conflict`; on a destroyed env `EnvGone`.

### Who runs the action

pyautogui, execute and launch run **inside the env as the desktop user `user`** (uid 1000, not root), with:

| | value (measured) |
|---|---|
| user | `user` (groups include `sudo`, but sudo needs a password: OSWorld's `password`) |
| working directory | `/home/user` |
| `HOME` | `/home/user` |
| `DISPLAY` | `:0` |
| `PATH` | `/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/snap/bin` |
| Python | the image's `python3` (3.10.12) with pyautogui |
| shell (`shell: true`) | `sh -c` (`SHELL` is `/bin/bash`) |
| network | none: loopback only, no DNS |
| CPU and memory | 4 vCPU (cpu.max) and 4 GiB (memory.max) |

**For RL**: `execute` gives a policy a shell in the env. It can solve file-system tasks without touching the GUI (a
`mv` instead of a rename in the file manager). If you train a GUI agent, do not expose `execute` to the policy; use it
only from your own harness (setup, probes).

## Observations

| call | returns | measured (c8i.4xlarge) |
|---|---|---|
| `env.screenshot()` | PNG bytes, 1920 x 1080, 8-bit RGB (no alpha), about 1.7 MB | 0.57 s |
| `env.observe()` | `Observation`: `.png`, `.width`, `.height`, `.meta` = `env`, `episode`, `state`, `verdict`, `task_id`, `instruction`, `steps`, `t` | 0.54 s |
| `env.step(a, screenshot=True)` | the step's result plus `.observation` | 0.74 s for a mouse move |
| `env.step(a)` | without a screenshot | 0.19 s for a mouse move (median of 10) |

Resize screenshots in your code if your model wants a smaller image (most VLMs do: 1920 x 1080 is ~2 M pixels).

## Episode timing (measured, one env, task e0df059f)

| operation | seconds |
|---|---|
| create (from the image, with task setup) | 1.8 |
| until the last gate resolved (`wait_valid`) | 4.0 more (the 5 s alive gate, counted from ready) |
| reset | 1.5 |
| fork(3) | 2.9 |
| verify (this task's checker) | 0.1 (other tasks' checkers take seconds) |

The shortest valid episode is therefore about 5 s; see validity.md.

## Tasks

`client.tasks(domain=None)` lists the pinned OSWorld checkout's tasks (id, domain, instruction); `client.task(id)` returns
one task's JSON (by id or an 8+ character prefix). Before you pick one, read its JSON: tasks with
`"evaluator": {"func": "infeasible"}` have no solution, and tasks whose `config` or evaluator download files need
outbound access from the host. Task setup runs at create and after every reset of an env created with the task; fork
children inherit the parent's set-up state.
