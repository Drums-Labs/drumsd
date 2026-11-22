# libc-follow.sh: the C library's development files follow the host's C library (sourced by install.sh; bash).
# shellcheck shell=bash
#
#   libc_dev_follow SNAPSHOT
#
# Every other package comes from the apt snapshot in install/lock.json. libc6-dev (and libc-dev-bin) cannot: each
# depends on libc6 of exactly its own version, and libc6 is already on the host. When the host image ships a libc6
# newer than the snapshot (the stock Ubuntu 24.04 AMI of 2026-11-20 has 2.39-0ubuntu8.12; snapshot 20261029T000000Z has
# 2.39-0ubuntu8.9), the snapshot's libc6-dev would need libc6 downgraded. apt does not downgrade an essential package it
# was not asked for, so the install stops with "held broken packages". Downgrading the host's C library is not an
# option either.
#
# So when the host's libc6 is newer than the snapshot's libc6-dev, this installs libc6-dev and libc-dev-bin at exactly
# the host's libc6 version from the host's own Ubuntu archive, then puts the snapshot lists back. build-essential
# (still from the snapshot) depends on libc6-dev without a version, so it takes the installed one. If the archive no
# longer carries that exact version (it moved on), it installs the archive's current libc6, libc-bin, libc6-dev and
# libc-dev-bin together. That is a libc security update on the host, and it is logged.
# When the host's libc6 is not newer than the snapshot, nothing changes: the snapshot supplies libc6-dev as before.
#
# Needs: retry (install/retry.sh), say (install.sh), dpkg-query, dpkg, apt-cache, apt-get.
libc_dev_follow() {
  local snap=$1 host have snapv log=${RETRY_LOG_DIR:-/tmp}/libc-follow.log
  host=$(dpkg-query -W -f='${Version}' libc6 2>/dev/null) || return 0
  [ -n "$host" ] || return 0
  have=$(dpkg-query -W -f='${db:Status-Abbrev}${Version}' libc6-dev 2>/dev/null || true)
  [ "$have" = "ii $host" ] && return 0                       # already matched (a rerun)
  snapv=$(apt-cache --snapshot "$snap" madison libc6-dev 2>/dev/null | awk -F'|' '{gsub(/ /, "", $2); print $2}' |
          sort -V | tail -1)
  if [ -n "$snapv" ] && dpkg --compare-versions "$host" le "$snapv"; then
    return 0                                                  # the snapshot can match the host: the normal path
  fi
  say "apt: the host's libc6 $host is newer than snapshot $snap's libc6-dev ${snapv:-(none)}: libc6-dev follows the host's libc6 (from the host's Ubuntu archive)"
  retry "apt update (archive, for libc6-dev)" apt-get update -q >> "$log" || { say "apt: see $log"; return 1; }
  if ! retry "apt install libc6-dev=$host" apt-get install -y -q --no-install-recommends \
         "libc6-dev=$host" "libc-dev-bin=$host" >> "$log"; then
    say "apt: the archive no longer has libc6-dev $host; installing its current libc6 and libc6-dev together (a libc update on this host)"
    retry "apt install libc6-dev (archive current)" apt-get install -y -q --no-install-recommends \
      libc6 libc-bin libc6-dev libc-dev-bin >> "$log" || { say "apt: see $log"; return 1; }
  fi
  say "apt: libc6 $(dpkg-query -W -f='${Version}' libc6), libc6-dev $(dpkg-query -W -f='${Version}' libc6-dev)"
  retry "apt update (snapshot $snap)" apt-get update -q --snapshot "$snap" >> "$log"
}
