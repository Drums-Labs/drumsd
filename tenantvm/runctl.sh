#!/usr/bin/env bash
# runctl.sh: convenience wrappers for the measurement, run on the metal host.
#   bare-token       -> prints the bare host's drums token
#   vm-token IDX     -> prints tenant VM IDX's drums token (over the tap)
#   bare PY ARGS...  -> run a staged python script against the bare host drumsd
#   vm IDX PY ARGS.. -> run a staged python script INSIDE tenant VM IDX against its drumsd
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; . "$HERE/pins.env" 2>/dev/null || true
case "${1:?}" in
  bare-token) sudo cat /etc/drums/token ;;
  vm-token) sudo "$HERE/gssh" "${2:?idx}" "sudo cat /etc/drums/token" ;;
  bare) shift; ~/dv/bin/python "$@" ;;
  vm) IDX=${2:?idx}; shift 2; sudo "$HERE/gssh" "$IDX" "cd ~/rel && ~/dv/bin/python $*" ;;
esac
