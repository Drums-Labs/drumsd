# Your own tasks: setup, apps, data and reward functions

Drums runs any task written in OSWorld's task format, not only the tasks in the pinned OSWorld checkout. A task is
a JSON object you pass to `client.create(task=...)` (a dict, or a path to a `.json` file). It names its setup steps
and how it is scored. With a **lab verifier**, the reward is a function you write in plain Python.

## The task format

```json
{"id": "acme-expense-01",
 "instruction": "In the Acme Ops app open in Chrome, submit a new expense: ...",
 "config": [ ...setup steps, run in order at create and after every reset... ],
 "evaluator": {"func": "drums_verifier", "verifier": "acme", "function": "expense_submitted",
               "options": {"amount_cents": 8450}}}
```

- `id`: letters, digits, `-` and `_`. It names the task's download cache.
- `config`: OSWorld's setup steps. The ones a lab usually needs:

| step | parameters | runs |
|---|---|---|
| `upload_file` | `files: [{"local_path": HOST_PATH, "path": DESKTOP_PATH}]` | copies a host file under `/var/lib/drums/assets` into the desktop (parent directories are created) |
| `download` | `files: [{"url": URL, "path": DESKTOP_PATH}]` | the host fetches the URL once (cached per task) and copies it in |
| `launch` | `command: [...]`, `shell: false` | starts a program in the desktop and returns at once (an app server, Chrome, a file) |
| `execute` | `command: [...]`, `shell: false` | runs a command in the desktop and waits; a non-zero exit fails the setup |
| `open` | `path: DESKTOP_PATH` | opens a file with its default app (e.g. LibreOffice Calc for `.xlsx`) |
| `sleep` | `seconds: N` | waits |

  `launch`, `execute` and `open` run as the desktop user `user` (uid 1000, not root), with no network: an app the
  task needs has to run from what the image has (`python3` 3.10 with its standard library, Chrome, LibreOffice,
  GIMP, VS Code, Thunderbird, VLC) plus the files you copy in.
- **Steps that need root.** The desktop user has no `sudo` (below). An `execute` or `command` step whose command runs
  `sudo` (OSWorld's `echo {CLIENT_PASSWORD} | sudo -S ...`, also in an evaluator's `postconfig`) is trusted
  provisioning: drumsd runs it in the env itself, as `user` but with `sudo` working for that step only, so the parts
  under `sudo` run as root and the rest as `user`, as on the VM. It runs before the env is handed out (or in the
  checker's postconfig), never while the agent acts. Its exit status is treated as OSWorld treats it (ignored, as on
  the VM). Steps without `sudo` go to the desktop as before.
- `evaluator`: a lab verifier (below), or any OSWorld evaluator (`func`, `result`, `expected`, `options`).

If a setup step fails, the env is left `down` and `create()` raises `TaskSetupFailed` with the step's error.

## Your files on the host (trusted setup)

`upload_file` reads `local_path` on the host as the unprivileged checker user `drums-osw`, and **only a regular file
under the assets directory** `/var/lib/drums/assets` (created by the installer; `osw.assets_dir` in drumsd.json
overrides it). Symlinks are resolved first, so a link out of the directory is refused too; any other path fails the
setup (`TaskSetupFailed`: "... is outside the assets directory"). The host's operator installs a task's files there:

```bash
sudo install -d -m 0755 /var/lib/drums/assets/acme/app /var/lib/drums/assets/acme/data
sudo install -m 0644 acme_app.py /var/lib/drums/assets/acme/app/ && sudo install -m 0644 q3_sales.xlsx /var/lib/drums/assets/acme/data/
```

Setup is trusted, the agent is not: nothing in this page gives the agent more than the desktop user's rights.

## No privilege escalation for the agent

The agent acts as `user`, and `user` gets no root in an env: it is in none of the administrator groups (`sudo`,
`admin`, `wheel`, and `lxd`/`docker`, which give root through a daemon), neither in its processes nor in the env's
`/etc/group` and `/etc/gshadow`, and every process it drives (the guest server, the desktop, everything they start)
runs with `no_new_privs`, so `sudo`, `su`, `pkexec` and file capabilities do nothing. `sudo -n true` and
`echo password | sudo -S true` both fail in an env.

- A task whose **setup** needs root is supported: that step is provisioning (above).
- A task that needs the **agent** to act as root is unsupported in this execution profile: `create()` raises
  `TaskUnsupported` (HTTP 422, code `task_unsupported`) and `GET /v1/tasks` lists it with `"supported": false` and
  the reason. In the pinned OSWorld: 5812b315 (create an SSH user), 94d95f96 (install Spotify), b6781586 (set the
  system time zone).

## Lab verifiers: your reward function in Python

The operator installs one Python file per verifier into `/etc/drums/verifiers/` (set `osw.verifiers_dir` in
drumsd.json to use another directory). The file and its directory must be **owned by root and not writable by group
or others**, or drumsd refuses to load it. A task names the verifier and a function, plus JSON `options`. It never
carries code.

```bash
sudo install -m 0644 -o root -g root acme.py /etc/drums/verifiers/acme.py
```

```python
# /etc/drums/verifiers/acme.py
import os, sqlite3

def expense_submitted(ctx, amount_cents):
    data = ctx.read_file("/home/user/acme/acme.db")        # bytes, or None if the file does not exist
    if data is None:
        return 0.0
    p = os.path.join(ctx.workdir, "acme.db")               # a private scratch directory for this one verify
    open(p, "wb").write(data)
    n = sqlite3.connect(p).execute("SELECT count(*) FROM expenses WHERE amount_cents=?", (amount_cents,)).fetchone()[0]
    return {"score": float(n == 1), "info": {"rows": n}}   # or just a number
```

- **Where it runs.** `env.verify()` runs the function on the host as `drums-osw`, outside the desktop. The agent
  cannot change the code that scores it. Do not put checker scripts inside the desktop: the agent runs as the
  same user that owns those files and can rewrite them.
- **What it can see.** `ctx.read_file(path)` returns a file's bytes. `ctx.run(command, shell=False)` runs a command in
  the desktop as `user` and returns `{"returncode", "output", "error"}`. Also: `ctx.options`, `ctx.task`, `ctx.workdir`.
  It sees only the desktop of the env being verified.
- **Python.** The checker venv (`/opt/drums/osw-venv`: OSWorld's checker dependencies, e.g. `openpyxl`, `pandas`,
  `requests`). Use the standard library or those packages.
- **The result.** A finite number (a `bool` counts as 0/1), or `{"score": number, "info": {...}}`. `info` comes back in
  `Reward.raw["checker"]["info"]`. An exception, a guest error while reading, or any other return value is a **checker
  error** (`Reward.checker_error`, `score` None). It is never a score. A file that does not exist is `None`, so you
  can score it as a valid 0.
- **Validity is unchanged.** The reward carries the episode's gates (docs/validity.md). A quarantined env is not scored.
- A task's evaluator may add OSWorld `postconfig` steps; they run before the function.

## Many episodes from one set-up state

Task setup runs at every create and reset. To pay for it once, set up one env, `state = env.save(ttl=0)`, and then
create every episode with `client.create(from_state=state)`, or `fork()` it. Children inherit the running app and its
data. Saved states live in the RAM-backed state dir (~0.6 GB each) and are lost on a reboot.

## Checklist for a new task

1. The app runs as `user` with no network and no `sudo`; its host files are under `/var/lib/drums/assets`.
2. `create(task=...)` comes up `valid` (`wait_valid()`), and a screenshot shows the start state.
3. The verifier returns 0 on the start state and 1 on a state you solved by hand (`env.step` actions, then `verify`).
4. A wrong solution scores 0, and the verifier never raises on any state the agent can reach.
