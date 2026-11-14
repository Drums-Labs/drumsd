# retry.sh: bounded retry with backoff for network steps of install.sh (sourced; bash).
# shellcheck shell=bash
#   retry LABEL CMD [ARGS...]    runs CMD up to DRUMS_RETRY_ATTEMPTS (5) times. Waits between attempts grow
#   15, 30, 45, 60, 60 s (DRUMS_RETRY_BASE_S x attempt, capped at DRUMS_RETRY_MAX_S). Each failure is logged with
#   its exit code and the last lines of its output; the output of every attempt is kept in $RETRY_LOG_DIR.
#   apt-get update can exit 0 after "W: Failed to fetch ... InRelease" (it falls back to old or no lists), so an
#   attempt whose output has a "Failed to fetch" or "E:" line counts as failed as well.
retry() {
  local label=$1; shift
  local n=${DRUMS_RETRY_ATTEMPTS:-5} base=${DRUMS_RETRY_BASE_S:-15} max=${DRUMS_RETRY_MAX_S:-60}
  local dir=${RETRY_LOG_DIR:-/tmp} i rc out wait
  for ((i = 1; i <= n; i++)); do
    out="$dir/retry-$(printf '%s' "$label" | tr -c 'A-Za-z0-9_-' _)-$i.log"
    "$@" > "$out" 2>&1
    rc=$?
    if [ $rc = 0 ] && ! grep -qE '^(E:|W: Failed to fetch)' "$out"; then
      cat "$out"
      [ $i -gt 1 ] && echo "$(date -u +%FT%TZ) [drums-install] $label succeeded on attempt $i of $n"
      return 0
    fi
    echo "$(date -u +%FT%TZ) [drums-install] $label attempt $i of $n failed (exit $rc): $(grep -E 'E:|Failed|rror' "$out" | tail -3 | tr '\n' ' ' | cut -c1-400)"
    [ $i -lt $n ] || break
    wait=$((base * i)); [ $wait -gt $max ] && wait=$max
    sleep "$wait"
  done
  return 1
}
