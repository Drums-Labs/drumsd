#!/usr/bin/env bash
# net.sh up NAME INDEX [mq|sq] | down NAME | rules : the host side of a tenant VM's network.
# Each tenant gets its own tap (no bridge): 10.200.INDEX.1/30 on the host, .2 in the guest. The host firewall (table inet
# t4, priority -10, so it runs before anything else) allows a tenant VM only NEW connections out to the public internet:
#   - nothing to 169.254.0.0/16 (instance metadata, the VPC DNS and time service), fe80::/10 or fd00:ec2::/32;
#   - nothing to private, shared or loopback ranges (other tenants, the VPC, the host's own services);
#   - nothing to the host itself (input from a tap: replies to the host's own ssh only);
#   - no IPv6 at all from a tap.
# T4_EGRESS=closed (rules) drops the internet accept too (a tenant with no egress).
set -euo pipefail
WAN=${WAN:-$(ip route show default | awk '{print $5; exit}')}
rules() {
  local acc="accept"; [ "${T4_EGRESS:-open}" = closed ] && acc="drop"
  nft -f - <<N
table inet t4
delete table inet t4
table inet t4 {
  set blocked4 { type ipv4_addr; flags interval; elements = { 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16,
                 172.16.0.0/12, 192.0.0.0/24, 192.168.0.0/16, 198.18.0.0/15, 224.0.0.0/3 }
  }
  chain input { type filter hook input priority -10; policy accept;
    iifname "tap-*" meta nfproto ipv6 drop
    iifname "tap-*" ct state established,related accept
    iifname "tap-*" counter drop
  }
  chain forward { type filter hook forward priority -10; policy accept;
    iifname "tap-*" meta nfproto ipv6 counter drop
    oifname "tap-*" meta nfproto ipv6 drop
    oifname "tap-*" ct state established,related accept
    oifname "tap-*" counter drop
    iifname "tap-*" ip daddr @blocked4 counter drop
    iifname "tap-*" oifname "$WAN" counter $acc
    iifname "tap-*" counter drop
  }
  chain postrouting { type nat hook postrouting priority 100; policy accept;
    ip saddr 10.200.0.0/16 oifname "$WAN" masquerade
  }
}
N
}
case "${1:?up|down|rules}" in
  up)
    # MODE mq (Cloud Hypervisor, multi-queue virtio-net) or sq (Firecracker opens the tap single-queue)
    N=${2:?name}; I=${3:?index}; Q=${4:-mq}; T=tap-$N
    sysctl -qw net.ipv4.ip_forward=1
    ip link del "$T" 2>/dev/null || true
    if [ "$Q" = mq ]; then ip tuntap add "$T" mode tap multi_queue; else ip tuntap add "$T" mode tap; fi
    ip addr flush dev "$T"; ip addr add 10.200.$I.1/30 dev "$T"; ip link set "$T" up
    sysctl -qw net.ipv6.conf.$T.disable_ipv6=1
    rules ;;
  down) ip link del "tap-${2:?name}" 2>/dev/null || true ;;
  rules) rules ;;
esac
