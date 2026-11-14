"""Real subprocess executor. Tests substitute a labelled fake."""

from __future__ import annotations

import datetime
import os
import subprocess
import time

from .contract import Command, ExecResult

is_fake = False


class SubprocessExecutor:
    is_fake = False

    def monotonic(self) -> float:
        return time.monotonic()

    def run(self, cmd: Command, default_timeout_s: float) -> ExecResult:
        timeout = cmd.timeout_s or default_timeout_s
        env = dict(os.environ)
        env.update({k: str(v) for k, v in cmd.env.items()})
        wall = datetime.datetime.now(datetime.timezone.utc).isoformat()
        t0 = time.monotonic()
        try:
            p = subprocess.run(list(cmd.argv), env=env, capture_output=True, text=True,
                               timeout=timeout)
            return ExecResult(p.returncode, p.stdout, p.stderr, t0, time.monotonic(), wall)
        except subprocess.TimeoutExpired as e:
            out = e.stdout.decode(errors="replace") if isinstance(e.stdout, bytes) else (e.stdout or "")
            err = e.stderr.decode(errors="replace") if isinstance(e.stderr, bytes) else (e.stderr or "")
            return ExecResult(None, out, err, t0, time.monotonic(), wall, timed_out=True,
                              error=f"timeout after {timeout}s")
        except FileNotFoundError as e:
            return ExecResult(None, "", "", t0, time.monotonic(), wall, error=f"tool_absent: {e}")
        except OSError as e:
            return ExecResult(None, "", "", t0, time.monotonic(), wall, error=f"os_error: {e}")
