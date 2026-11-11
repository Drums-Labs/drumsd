#!/usr/bin/env bash
# run.sh NAME VMM VCPUS MEM_MIB CPUSET [INDEX]: boot tenant VM NAME ($T4/NAME.raw) with VMM = ch | fc.
# The VMM runs in its own systemd scope pinned to CPUSET (whole physical cores: both hyperthreads of each core), so two
# tenants never share a core. Serial console: $T4/NAME.serial. CH API socket $T4/NAME.ch.sock; FC $T4/NAME.fc.sock.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"; . "$HERE/pins.env"
N=${1:?name}; V=${2:?ch|fc}; C=${3:?vcpus}; M=${4:?mem MiB}; S=${5:?cpuset}; I=${6:-}
cd "$T4"; Q=mq; [ "$V" = fc ] && Q=sq; [ -n "$I" ] && "$HERE/net.sh" up "$N" "$I" "$Q"
MAC=$(printf '06:00:0a:c8:%02x:02' "${I:-1}")
CMD="console=ttyS0 root=/dev/vda rw reboot=k panic=1 net.ifnames=0 biosdevname=0 quiet"
rm -f "$N.serial" "$N.ch.sock" "$N.fc.sock"
case "$V" in
  ch) systemd-run --unit "t4vm-$N" -p AllowedCPUs="$S" -p Delegate=no --collect \
        "$T4/bin/cloud-hypervisor" --api-socket "path=$T4/$N.ch.sock" --kernel "$T4/vmlinux" --cmdline "$CMD" \
        --cpus "boot=$C" --memory "size=${M}M,thp=on" --disk "path=$T4/$N.raw,image_type=raw,num_queues=4,queue_size=256" \
        --net "tap=tap-$N,mac=$MAC,num_queues=4" --serial "file=$T4/$N.serial" --console off --rng src=/dev/urandom ;;
  fc) cat > "$N.fc.json" <<J
{ "boot-source": { "kernel_image_path": "$T4/vmlinux", "boot_args": "$CMD" },
  "drives": [ { "drive_id": "root", "path_on_host": "$T4/$N.raw", "is_root_device": true, "is_read_only": false, "io_engine": "Async" } ],
  "machine-config": { "vcpu_count": $C, "mem_size_mib": $M, "smt": false },
  "network-interfaces": [ { "iface_id": "eth0", "guest_mac": "$MAC", "host_dev_name": "tap-$N" } ],
  "entropy": {} }
J
      systemd-run --unit "t4vm-$N" -p AllowedCPUs="$S" --collect -p StandardOutput="file:$T4/$N.serial" \
        "$T4/bin/firecracker" --api-sock "$T4/$N.fc.sock" --config-file "$T4/$N.fc.json" ;;
  *) echo "VMM must be ch or fc" >&2; exit 2 ;;
esac
echo "t4vm-$N started ($V, $C vCPU, $M MiB, cpus $S)"
