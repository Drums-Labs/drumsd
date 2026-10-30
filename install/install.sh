#!/usr/bin/env bash
# Drums one-command install: a fresh Ubuntu 24.04 x86_64 host -> a running drumsd with the drums-criu build, the OSWorld
# image as a clean checkpoint, OSWorld's checkers, and an API token.
#
#   sudo ./install.sh [--remote] [--port 7070] [--host-usd-per-hour X] [--state-tmpfs-gib N] [--swap-mib N] [--no-procview]
#
# Run it from an unpacked release (make-release.sh): this directory holds install/, sources/*.bundle and SHA256SUMS.
# Every input is pinned in install/lock.json (apt snapshot + versions, file URLs + sha256, source commits, Rust toolchain,
# hashed Python lock) and checked before use; the first mismatch stops the install.
# The API is bound to 127.0.0.1 unless --remote is given. Prints the URL and the token at the end.
# Log: /var/log/drums-install.log. Receipt (versions, hashes, phase times): /var/lib/drums/install-receipt.json.
set -euo pipefail

REL="$(cd "$(dirname "$0")" && pwd)"
LOCK="$REL/install/lock.json"
REMOTE=0; PORT=7070; PRICE=""; TMPFS_GIB=""; SWAP_MIB=0; PROCVIEW=1
while [ $# -gt 0 ]; do
  case "$1" in
    --remote) REMOTE=1; shift;;
    --port) PORT="${2:?}"; shift 2;;
    --host-usd-per-hour) PRICE="${2:?}"; shift 2;;
    --state-tmpfs-gib) TMPFS_GIB="${2:?}"; shift 2;;
    --swap-mib) SWAP_MIB="${2:?}"; shift 2;;
    --no-procview) PROCVIEW=0; shift;;   # labelled A/B control only: envs see the host's memory and CPU files   # swap per env (needs host swap, e.g. zram); default 0
    -h|--help) sed -n 2,12p "$0"; exit 0;;
    *) echo "unknown option $1" >&2; exit 2;;
  esac
done

LOG=/var/log/drums-install.log
D=/var/lib/drums; O=/opt/drums; E=/etc/drums
mkdir -p "$D" "$O" "$E"
exec 9> "$D/install.lock"
flock -n 9 || { echo "another install.sh is running (lock $D/install.lock)" >&2; exit 1; }
touch "$LOG"; chmod 0600 "$LOG"      # the log ends with the API token
exec > >(tee -a "$LOG") 2>&1
BG=""
trap '[ -n "$BG" ] && kill $BG 2>/dev/null; true' EXIT
T0=$(date +%s)
PH="$D/install-phases.jsonl"; : > "$PH"
ts() { date -u +%FT%TZ; }
say() { echo "$(ts) [drums-install] $*"; }
die() { echo "$(ts) [drums-install] FAILED: $*" >&2; exit 1; }
phase() { # phase NAME START_EPOCH
  local now; now=$(date +%s); echo "{\"phase\":\"$1\",\"seconds\":$((now - $2)),\"end_utc\":\"$(ts)\"}" >> "$PH"; say "phase $1 done in $((now - $2)) s"; }
lk() { python3 -c "import json,sys; d=json.load(open('$LOCK')); v=d
for k in sys.argv[1].split('.'): v=v[k]
print(v if not isinstance(v,(dict,list)) else json.dumps(v))" "$1"; }

# ------------------------------------------------------------------------------------------------ 0. preflight
t=$(date +%s)
[ "$(id -u)" = 0 ] || die "run as root (sudo)"
. /etc/os-release
[ "$ID" = ubuntu ] && [ "$VERSION_ID" = 24.04 ] || die "Ubuntu 24.04 required, found $PRETTY_NAME"
[ "$(uname -m)" = x86_64 ] || die "x86_64 required"
[ "$(stat -fc %T /sys/fs/cgroup)" = cgroup2fs ] || die "cgroup v2 required"
FREE_GIB=$(( $(df --output=avail -B1 / | tail -1) / 1073741824 ))
[ "$FREE_GIB" -ge "$(lk host.min_free_disk_gib)" ] || die "need $(lk host.min_free_disk_gib) GiB free on /, have $FREE_GIB"
MEM_GIB=$(( $(awk '/MemTotal/{print $2}' /proc/meminfo) / 1048576 ))
[ "$MEM_GIB" -ge "$(lk host.min_mem_gib)" ] || die "need $(lk host.min_mem_gib) GiB RAM, have $MEM_GIB"
(cd "$REL" && sha256sum -c --quiet SHA256SUMS) || die "release files do not match SHA256SUMS"
say "host $(hostname) kernel $(uname -r) (tested: $(lk host.tested_kernel)), ${MEM_GIB} GiB RAM, ${FREE_GIB} GiB free"
phase preflight $t

# ------------------------------------------------------------------------------------------------ 1. apt (snapshot-pinned)
t=$(date +%s)
export DEBIAN_FRONTEND=noninteractive
SNAP=$(lk apt.snapshot)
mapfile -t PKGS < <(python3 -c "import json; d=json.load(open('$LOCK'))['apt']['packages']; [print(f'{k}={v}') for k,v in d.items()]")
# the snapshot service and the mirrors fail transiently (503, "mirror sync in progress"): bounded retry with backoff,
# every failed attempt logged and its output kept; the snapshot pin and the version check below are unchanged
. "$REL/install/retry.sh"
RETRY_LOG_DIR="$D"; mkdir -p "$D"
retry "apt update (snapshot $SNAP)" apt-get update -qq --snapshot "$SNAP" || die "apt update (snapshot $SNAP), ${DRUMS_RETRY_ATTEMPTS:-5} attempts"
retry "apt install" apt-get install -y -qq --no-install-recommends --snapshot "$SNAP" "${PKGS[@]}" > "$D/apt-install.log" 2>&1 || { tail -30 "$D/apt-install.log"; die "apt install"; }
for pv in "${PKGS[@]}"; do
  p=${pv%%=*}; want=${pv#*=}; got=$(dpkg-query -W -f='${Version}' "$p" 2>/dev/null || true)
  [ "$got" = "$want" ] || die "package $p is $got, pinned $want"
done
dpkg-query -W -f='${Package}\t${Version}\n' | sort > "$D/packages-installed.txt"
phase apt $t

# ------------------------------------------------------------------------------------------------ 2. kernel settings
t=$(date +%s)
cat > /etc/sysctl.d/60-drums-slots.conf <<'EOF'
# CRIU creates every restored inotify instance as uid 0, so every slot on the host shares one per-user count.
# 8192 stopped a host at about 300 GNOME slots. An instance costs ~1 KiB, a watch ~0.1 KiB, both charged to the
# creating task's memory cgroup (the slot's), so memory.max still bounds them (drums-info oom-inotify 20261030T0344Z).
fs.inotify.max_user_instances = 131072
fs.inotify.max_user_watches = 4194304
EOF
sysctl -q --system
printf 'nbd\noverlay\n' > /etc/modules-load.d/drums.conf
modprobe nbd max_part=16 || die "modprobe nbd"; modprobe overlay || die "modprobe overlay"
phase kernel $t

# ------------------------------------------------------------------------------------------------ 3. sources (pinned commits)
t=$(date +%s)
mkdir -p "$O/src"
src() { # src NAME COMMIT
  local n=$1 c=$2 dst="$O/src/$1"
  if [ ! -d "$dst/.git" ]; then git clone -q "$REL/sources/$n.bundle" "$dst" 2>/dev/null || git init -q "$dst"; fi
  git -C "$dst" fetch -q "$REL/sources/$n.bundle" "+refs/heads/*:refs/remotes/bundle/*" || die "fetch $n bundle"
  git -C "$dst" -c advice.detachedHead=false checkout -q --force "$c" || die "$n: commit $c not in its bundle"
  [ "$(git -C "$dst" rev-parse HEAD)" = "$c" ] || die "$n HEAD is not $c"
  git -C "$dst" clean -qfdx -e target
  say "$n at $c"
}
src drums-criu "$(lk sources.drums-criu.commit)"
src drums-lab "$(lk sources.drums-lab.commit)"
src drumsd "$(cat "$REL/RELEASE_COMMIT")"
OSW_C=$(lk sources.osworld.commit)
if [ ! -d "$O/src/OSWorld/.git" ]; then git init -q "$O/src/OSWorld"; fi
git -C "$O/src/OSWorld" fetch -q --depth 1 "$(lk sources.osworld.repo)" "$OSW_C" || die "fetch OSWorld $OSW_C"
git -C "$O/src/OSWorld" -c advice.detachedHead=false checkout -q --force FETCH_HEAD
[ "$(git -C "$O/src/OSWorld" rev-parse HEAD)" = "$OSW_C" ] || die "OSWorld HEAD is not $OSW_C"
phase sources $t

# ------------------------------------------------------------------------------------------------ 4. image (download, verify, extract) - in the background
t_img=$(date +%s)
IMG="$D/image"; mkdir -p "$IMG"
(
  set -e
  Q="$IMG/Ubuntu.qcow2"; QSHA=$(lk files.osworld_image_qcow2.sha256)
  if [ -f "$Q" ] && [ -f "$IMG/qcow2.verified" ] && [ "$(cat "$IMG/qcow2.verified")" = "$QSHA $(stat -c %s.%Y "$Q")" ]; then echo "image already verified"; exit 0; fi
  Z="$IMG/Ubuntu.qcow2.zip"; ZSHA=$(lk files.osworld_image_zip.sha256)
  if ! { [ -f "$Z" ] && echo "$ZSHA  $Z" | sha256sum -c --quiet - 2>/dev/null; }; then
    curl -sSfL --retry 5 --retry-delay 10 -C - -o "$Z" "$(lk files.osworld_image_zip.url)"
  fi
  [ "$(stat -c %s "$Z")" -le "$(lk files.osworld_image_zip.max_bytes)" ] || { echo "zip larger than pinned max"; exit 1; }
  echo "$ZSHA  $Z" | sha256sum -c --quiet - || { echo "zip sha256 mismatch"; exit 1; }
  unzip -p "$Z" Ubuntu.qcow2 > "$Q.part"
  echo "$QSHA  $Q.part" | sha256sum -c --quiet - || { echo "qcow2 sha256 mismatch"; exit 1; }
  mv "$Q.part" "$Q"; chmod 0444 "$Q"; rm -f "$Z"
  echo "$QSHA $(stat -c %s.%Y "$Q")" > "$IMG/qcow2.verified"
  echo "image verified"
) > "$D/image.log" 2>&1 &
IMG_PID=$!; BG=$IMG_PID

# ------------------------------------------------------------------------------------------------ 5. CRIU build
t=$(date +%s)
make -C "$O/src/drums-criu" -j"$(nproc)" criu > "$D/criu-build.log" 2>&1 || { tail -30 "$D/criu-build.log"; die "criu build"; }
install -D -m 0755 "$O/src/drums-criu/criu/criu" "$O/criu/bin/criu"
"$O/criu/bin/criu" check > "$D/criu-check.log" 2>&1 || { cat "$D/criu-check.log"; die "criu check"; }
CRIU_SHA=$(sha256sum "$O/criu/bin/criu" | cut -d' ' -f1)
say "criu $("$O/criu/bin/criu" --version | head -1 | tr '\n' ' ') sha256 $CRIU_SHA (reference build $(lk sources.drums-criu.reference_binary_sha256 | cut -c1-12)...)"
phase criu $t

# ------------------------------------------------------------------------------------------------ 6. drumsd build (pinned toolchain, --locked)
t=$(date +%s)
export RUSTUP_HOME="$O/rust" CARGO_HOME="$O/rust"
if [ ! -x "$O/rust/bin/cargo" ]; then
  curl -sSfL -o /tmp/rustup-init "$(lk files.rustup_init.url)"
  echo "$(lk files.rustup_init.sha256)  /tmp/rustup-init" | sha256sum -c --quiet - || die "rustup-init sha256 mismatch"
  chmod +x /tmp/rustup-init && /tmp/rustup-init -y -q --no-modify-path --profile minimal --default-toolchain "$(lk rust_toolchain)" > "$D/rustup.log" 2>&1 || die "rustup"
fi
(cd "$O/src/drumsd" && "$O/rust/bin/cargo" build -q --release --locked) > "$D/drumsd-build.log" 2>&1 || { tail -30 "$D/drumsd-build.log"; die "drumsd build"; }
install -D -m 0755 "$O/src/drumsd/target/release/drumsd" "$O/bin/drumsd"
install -D -m 0644 "$O/src/drumsd/osw/drums_osw.py" "$O/osw/drums_osw.py"
# the SDK as a wheel any user can install (pip install /opt/drums/sdk/drums-*.whl): the release's own, checked above
rm -rf "$O/sdk"; install -d -m 0755 "$O/sdk"
for w in "$REL"/sdk/drums-*-py3-none-any.whl; do [ -f "$w" ] && install -m 0644 "$w" "$O/sdk/"; done
ls "$O"/sdk/drums-*.whl > /dev/null 2>&1 || die "the release has no SDK wheel (sdk/drums-*.whl)"
phase drumsd $t

# ------------------------------------------------------------------------------------------------ 7. OSWorld checkers (hashed Python lock, unprivileged user)
t=$(date +%s)
id drums-osw >/dev/null 2>&1 || useradd --system --no-create-home --home-dir "$D/osw-cache" --shell /usr/sbin/nologin drums-osw
mkdir -p "$D/osw-cache" && chown drums-osw:drums-osw "$D/osw-cache" && chmod 0750 "$D/osw-cache"
[ -x "$O/osw-venv/bin/python" ] || python3 -m venv "$O/osw-venv"
"$O/osw-venv/bin/pip" install -q --no-deps --require-hashes -r "$O/src/drumsd/install/osw-requirements.lock" \
  --extra-index-url "$(lk sources.osworld.extra_index)" > "$D/pip.log" 2>&1 || { tail -30 "$D/pip.log"; die "pip install (hashed lock)"; }
"$O/osw-venv/bin/pip" freeze > "$D/osw-venv-freeze.txt"
(cd "$O/src/OSWorld" && runuser -u drums-osw -- env HOME="$D/osw-cache" "$O/osw-venv/bin/python" -c "import desktop_env.desktop_env, desktop_env.evaluators.metrics, desktop_env.evaluators.getters; print('osworld import ok')") > "$D/osw-import.log" 2>&1 || { tail -20 "$D/osw-import.log"; die "OSWorld checkers do not import"; }
phase osworld $t

# ------------------------------------------------------------------------------------------------ 8. wait for the image, attach it read-only, inspect the root
wait $IMG_PID || { cat "$D/image.log"; die "image download/verify"; }
BG=""
phase image_download_verify $t_img
t=$(date +%s)
LAB="$O/src/drums-lab/benchmark"
export PYTHONPATH="$LAB" PYTHONDONTWRITEBYTECODE=1
mkdir -p /mnt/osworld
if ! findmnt /mnt/osworld >/dev/null; then
  python3 -m sharedos_rootfs.image attach --image "$IMG/Ubuntu.qcow2" --expected-sha256 "$(lk files.osworld_image_qcow2.sha256)" \
    --mountpoint /mnt/osworld --state "$D/image-mount.json" > "$D/image-attach.log" 2>&1 || { cat "$D/image-attach.log"; die "image attach"; }
fi
rm -f "$E/inspection.new.json"
python3 -m sharedos_rootfs.rootfs_inspect /mnt/osworld --out "$E/inspection.new.json" --mount-state "$D/image-mount.json" > "$D/inspect.log" 2>&1 || { tail -20 "$D/inspect.log"; die "rootfs inspection"; }
mv "$E/inspection.new.json" "$E/inspection.json"
mkdir -p "$D/packages"
curl -sSfL -o "$D/packages/dummy.deb" "$(lk files.xorg_dummy_deb.url)"
echo "$(lk files.xorg_dummy_deb.sha256)  $D/packages/dummy.deb" | sha256sum -c --quiet - || die "dummy deb sha256 mismatch"
phase image_attach $t

# ------------------------------------------------------------------------------------------------ 9. state, configs, token, service
t=$(date +%s)
[ -n "$TMPFS_GIB" ] || TMPFS_GIB=$(( MEM_GIB / 2 ))
mkdir -p "$D/state" "$D/journal"
findmnt "$D/state" >/dev/null || mount -t tmpfs -o size=${TMPFS_GIB}G,mode=0700 tmpfs "$D/state"
chmod 0700 "$D/state"
if [ -z "$PRICE" ]; then
  ITYPE=$(TOK=$(curl -sf -m 2 -X PUT -H "X-aws-ec2-metadata-token-ttl-seconds: 60" http://169.254.169.254/latest/api/token) && curl -sf -m 2 -H "X-aws-ec2-metadata-token: $TOK" http://169.254.169.254/latest/meta-data/instance-type || true)
  PRICE=$(python3 -c "import json; print(json.load(open('$LOCK'))['prices_usd_per_hour'].get('$ITYPE', 0))")
  say "host price: $PRICE USD/h (instance type '${ITYPE:-unknown}', pinned on-demand table; pass --host-usd-per-hour to set yours)"
fi
IMGSHA=$(lk files.osworld_image_qcow2.sha256)
cat > "$E/rootfs-config.json" <<EOF
{"state_dir": "$D/state", "lower": "/mnt/osworld", "inspection": "$E/inspection.json", "image_sha256": "$IMGSHA",
 "vcpus": $(lk runtime.vcpus), "memory_mib": $(lk runtime.memory_mib), "screen": "$(lk runtime.screen)", "profile": "gnome_x11",
 "service_base_port": 25000, "guest_service_port": 5000, "display_stage_deb": "$D/packages/dummy.deb",
 "procview_dir": $([ "$PROCVIEW" = 1 ] && echo '"/run/drums/procview"' || echo null)}
EOF
[ -s "$E/token" ] || (umask 077; python3 -c "import secrets; print(secrets.token_urlsafe(32))" > "$E/token")
chmod 0600 "$E/token"
# concurrent CRIU restores: 4 on the 16-vCPU host this was tuned on; one per 4 vCPUs on larger hosts (at 64 vCPU
# and 64 envs, 4 at a time queued fork children and resets for tens of seconds)
RESTORES=$(( $(nproc) / 4 )); [ "$RESTORES" -ge 4 ] || RESTORES=4
LISTEN="127.0.0.1:$PORT"; ALLOW=false
[ "$REMOTE" = 1 ] && { LISTEN="0.0.0.0:$PORT"; ALLOW=true; }
OSW_UID=$(id -u drums-osw); OSW_GID=$(id -g drums-osw)
cat > "$E/drumsd.json" <<EOF
{"listen": "$LISTEN", "allow_remote": $ALLOW, "token_file": "$E/token",
 "state_dir": "$D/state", "journal_dir": "$D/journal", "lower": "/mnt/osworld", "image_sha256": "$IMGSHA",
 "py_src": "$LAB", "py_config": "$E/rootfs-config.json", "criu": "$O/criu/bin/criu", "nested_pidns": true,
 "vcpus": $(lk runtime.vcpus), "memory_mib": $(lk runtime.memory_mib), "swap_mib": $SWAP_MIB, "procview_dir": "/run/drums/procview", "port_base": 25000, "guest_port": 5000,
 "extra_ports": [[27500, 9222], [30000, 8080]], "slot_range": [100, 2000], "max_concurrent_restores": $RESTORES,
 "natural_exits": $(lk runtime.natural_exits), "alive_after_s": 5.0, "max_attempts": 3, "warm_upper": true,
 "host_usd_per_hour": $PRICE,
 "osw": {"python": "$O/osw-venv/bin/python", "helper": "$O/osw/drums_osw.py", "osworld_dir": "$O/src/OSWorld",
         "cache_dir": "$D/osw-cache", "uid": $OSW_UID, "gid": $OSW_GID}}
EOF
# Per-env memory/CPU view: apps in an env see the env's own memory, swap and CPU count (/proc/meminfo, cpuinfo, stat,
# swaps, /sys/devices/system/cpu/{online,possible,present} and sysinfo(2)), not the host's. Its FUSE connection is kept
# in systemd's fd store, so a restart of this service does not break the binds of running envs.
cat > /etc/systemd/system/drums-procview.service <<EOF
[Unit]
Description=Drums per-env memory/CPU view (FUSE + sysinfo program)
Before=drumsd.service
[Service]
Type=notify
NotifyAccess=main
ExecStart=$O/bin/drumsd procview --mount /run/drums/procview
FileDescriptorStoreMax=4
FileDescriptorStorePreserve=restart
Restart=always
RestartSec=0
[Install]
WantedBy=multi-user.target
EOF
cat > /etc/systemd/system/drumsd.service <<EOF
[Unit]
Description=drumsd (Drums desktop-slot control plane)
After=network-online.target drums-procview.service
Wants=drums-procview.service
[Service]
ExecStart=$O/bin/drumsd serve --config $E/drumsd.json
Restart=always
RestartSec=1
# slots, template servers and restores live in their own cgroups; a restart of drumsd must never kill them
KillMode=process
# each env holds listeners, guest connections and pidfds: the default soft limit (1024) ran out at 64 envs
LimitNOFILE=1048576
Environment=RUST_LOG=info
# glibc malloc: drumsd reads 6-20 MB CRIU logs on up to 1024 blocking threads; with per-thread arenas and the dynamic
# mmap threshold the freed buffers stayed in ~450 arenas (RSS 10 GB after 25 min at 128 envs). Large buffers go to
# mmap (returned on free) and the arena count is capped.
Environment=MALLOC_ARENA_MAX=8 MALLOC_MMAP_THRESHOLD_=1048576
[Install]
WantedBy=multi-user.target
EOF
# OSWorld checker fork server: imports OSWorld once; drumsd's per-call helper hands it each setup/evaluate (and does
# the work itself if the server is not there)
cat > /etc/systemd/system/drums-osw.service <<EOF
[Unit]
Description=Drums OSWorld checker fork server (imports OSWorld once, one forked worker per setup/evaluate)
Before=drumsd.service
[Service]
User=drums-osw
Group=drums-osw
RuntimeDirectory=drums-osw
RuntimeDirectoryMode=0750
WorkingDirectory=$O/src/OSWorld
Environment=HOME=$D/osw-cache PATH=/usr/bin:/bin PYTHONDONTWRITEBYTECODE=1 OSWORLD_DIR=$O/src/OSWorld OMP_NUM_THREADS=1 OPENBLAS_NUM_THREADS=1 MKL_NUM_THREADS=1
ExecStart=$O/osw-venv/bin/python -I $O/osw/drums_osw.py serve --osworld-dir $O/src/OSWorld --sock /run/drums-osw/osw.sock
Restart=always
RestartSec=1
LimitNOFILE=1048576
[Install]
WantedBy=multi-user.target
EOF
systemctl daemon-reload
systemctl enable -q drums-osw.service
systemctl restart drums-osw.service
if [ "$PROCVIEW" = 1 ]; then
  systemctl unmask -q --runtime drums-procview.service 2>/dev/null || true
  systemctl enable -q drums-procview.service
  # a running view keeps serving across this restart (the fd store carries its connection to the new binary)
  systemctl restart drums-procview.service || { journalctl -u drums-procview --no-pager | tail -20; die "drums-procview did not start"; }
  grep -q ' /run/drums/procview .* - fuse.drums-procview ' /proc/self/mountinfo || die "the per-env view is not mounted at /run/drums/procview"
else
  say "per-env memory/CPU view OFF (--no-procview): envs read the host's /proc/meminfo, cpuinfo and sysinfo(2)"
  # masked for this boot: drumsd.service Wants= it and would start it again
  systemctl disable -q --now drums-procview.service 2>/dev/null || true
  systemctl mask -q --runtime drums-procview.service
  rm -f /sys/fs/bpf/drums-procview-sysinfo
  umount -l /run/drums/procview 2>/dev/null || true
fi
for i in $(seq 1 60); do [ -S /run/drums-osw/osw.sock ] && break; sleep 0.5; done
[ -S /run/drums-osw/osw.sock ] || { journalctl -u drums-osw --no-pager | tail -20; die "OSWorld checker server did not start"; }
phase config $t

# ------------------------------------------------------------------------------------------------ 10. clean checkpoint of the image, then drumsd
t=$(date +%s)
systemctl stop drumsd 2>/dev/null || true
python3 "$O/src/drumsd/install/bringup.py" --criu-dir "$O/criu/bin" --lab "$LAB" --rootfs-config "$E/rootfs-config.json" \
  --drumsd-config "$E/drumsd.json" --image-id "$(lk runtime.image_id)" --record "$D/bringup.json" || die "bring-up of the clean checkpoint"
phase bringup $t

T1=$(date +%s)
python3 - "$D/install-receipt.json" "$PH" "$LOCK" "$CRIU_SHA" "$((T1 - T0))" "$REL/RELEASE_COMMIT" <<'PY'
import json, sys, platform, subprocess
out, ph, lock, criu_sha, total, relc = sys.argv[1:]
json.dump({"total_seconds": int(total), "phases": [json.loads(l) for l in open(ph)], "lock": json.load(open(lock)),
           "criu_sha256": criu_sha, "drumsd_commit": open(relc).read().strip(), "kernel": platform.release(),
           "drumsd_sha256": subprocess.run(["sha256sum", "/opt/drums/bin/drumsd"], capture_output=True, text=True).stdout.split()[0]},
          open(out, "w"), indent=1)
PY
URL="http://127.0.0.1:$PORT"; [ "$REMOTE" = 1 ] && URL="http://$(hostname -I | awk '{print $1}'):$PORT"
cat <<EOF

================================================================================================
 Drums is running.  ($((T1 - T0)) s)
   URL:    $URL
   token:  $(cat "$E/token")         (also in $E/token, root only)
   image:  $(lk runtime.image_id)
 From another machine:  ssh -L $PORT:127.0.0.1:$PORT <this host>   then use http://127.0.0.1:$PORT
   SDK (any user):  python3 -m venv ~/drums-venv && ~/drums-venv/bin/pip install $(ls "$O"/sdk/drums-*.whl)
   import drums; c = drums.connect("$URL", "<token>"); env = c.create(image="$(lk runtime.image_id)")
================================================================================================
EOF
