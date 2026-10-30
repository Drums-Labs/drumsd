"""A minimal computer-use RL loop on Drums that cannot leak envs or saved states.

    python examples/rl_loop.py --task e0df059f --episodes 20 [--n 4]

Per episode: reset the parent env to the task's start, take a prefix of actions, fork the reached state into n
children, let each child continue with its own (here random) actions, verify each child, and keep only valid rewards.
Replace `policy()` with your model. Cleanup is by construction:
- `with drums.connect() as client` closes every env this client created when the block ends, also on Ctrl-C,
  SIGTERM and normal exit;
- `fork(..., keep_state=False)` makes drumsd delete the fork's saved state when its last child closes;
- `with ... as kids` closes the children at the end of each episode;
- leases (`idle_timeout`) let drumsd reap whatever a `kill -9` left behind.
"""
from __future__ import annotations

import argparse
import random
import time

import drums


def policy(obs: drums.Observation, rng: random.Random):
    """A stand-in for your model: a random click on the 1920x1080 screen (pixels, origin top left)."""
    x, y = rng.randrange(obs.width or 1920), rng.randrange(obs.height or 1080)
    return f"pyautogui.click({x}, {y})"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--task", default="e0df059f", help="OSWorld task id (or unique prefix of 8+ characters)")
    ap.add_argument("--episodes", type=int, default=20)
    ap.add_argument("--n", type=int, default=4, help="children per fork")
    ap.add_argument("--prefix-steps", type=int, default=2)
    ap.add_argument("--child-steps", type=int, default=3)
    ap.add_argument("--idle-timeout", type=float, default=300, help="lease: drumsd reaps envs unused this long")
    a = ap.parse_args()
    rng = random.Random(0)
    with drums.connect() as client:                       # DRUMS_URL, DRUMS_TOKEN
        client.reset_metrics()                            # the cost below is this run's, not since drumsd started
        task = client.task(a.task)
        parent = client.create(task=task, idle_timeout=a.idle_timeout)
        batch = []
        for ep in range(a.episodes):
            parent.wait_valid()                           # the episode's last gate (5 s after ready) passed
            for _ in range(a.prefix_steps):
                parent.step(policy(parent.observe(), rng))
            with parent.fork(a.n, keep_state=False, idle_timeout=a.idle_timeout) as kids:
                for k in kids:
                    if not k.valid():                     # wait out the child's 5 s gate before acting (docs/validity.md)
                        print(f"episode {ep} child {k.id}: invalid at start, dropped")
                        continue
                    for _ in range(a.child_steps):
                        try:
                            k.step(policy(k.observe(), rng))
                        except drums.ActionFailed:        # the action ran and failed: a legitimate outcome
                            pass
                    r = k.verify(strict=False)
                    if r.valid:
                        batch.append(r.score)             # train on these only
                    else:
                        print(f"episode {ep} child {k.id}: dropped ({'; '.join(r.invalid_reasons)})")
            print(f"episode {ep}: {len(batch)} valid rewards so far, mean {sum(batch) / max(len(batch), 1):.2f}")
            parent.reset()
        print("cost:", client.metrics()["cost"])


if __name__ == "__main__":
    t0 = time.time()
    main()
    print(f"done in {time.time() - t0:.0f} s")
