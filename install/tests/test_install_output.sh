#!/usr/bin/env bash
# install.sh output: progress lines while a long step runs (also when its output goes to a log), and the API token is
# never printed (the banner names the file). Runs anywhere with bash; no root, nothing installed.
set -euo pipefail
HERE="$(cd "$(dirname "$0")/.." && pwd)"; I="$HERE/install.sh"; fail=0
ok() { echo "PASS $*"; }; bad() { echo "FAIL $*"; fail=1; }
T=$(mktemp -d); trap 'rm -rf "$T"' EXIT
# the helpers, exactly as install.sh defines them
{ echo 'say() { echo "[t] $*"; }'; sed -n '/^heartbeat() {/,/^}/p' "$I"; grep '^with_progress() ' "$I"; } > "$T/h.sh"
. "$T/h.sh"
exec 8>"$T/terminal"
DRUMS_HEARTBEAT_S=1 with_progress "slow step" bash -c 'echo step-output; sleep 3.5' > "$T/step.log" 2>&1 && rc=0 || rc=$?
n=$(grep -c '\.\.\. slow step: ' "$T/terminal" || true)
[ "$rc" = 0 ] && [ "$n" -ge 2 ] && ok "progress lines on the terminal while the step runs ($n)" || bad "progress: rc=$rc lines=$n"
grep -q step-output "$T/step.log" && ! grep -q 'slow step' "$T/step.log" && ok "the step's own output stays in its log" || bad "step log"
DRUMS_HEARTBEAT_S=1 with_progress "failing step" bash -c 'exit 7' > /dev/null 2>&1 && rc=0 || rc=$?
[ "$rc" = 7 ] && ok "the step's exit status is returned ($rc)" || bad "exit status $rc"
# the token: written to the 0600 file only
if grep -nE 'cat "?\$E/token"?\)' "$I" | grep -v 'sudo cat' ; then bad "install.sh prints the token"; else ok "install.sh never prints the token"; fi
grep -q 'sudo cat $E/token' "$I" && ok "the banner names the token file" || bad "banner"
grep -q 'not byte-reproducible' "$I" && ok "the CRIU checksum note is printed" || bad "criu note"
exit $fail
