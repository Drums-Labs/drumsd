#!/usr/bin/env bash
# Live check of egress.py in a throwaway NAT'd network namespace on the host: metadata blocked, allow-list enforced.
set -euo pipefail
NS=t4egr; WAN=$(ip route show default | awk '{print $5; exit}')
cleanup(){ ip netns del $NS 2>/dev/null||true; ip link del t4egr-h 2>/dev/null||true; iptables -t nat -D POSTROUTING -s 10.231.0.0/30 -o $WAN -j MASQUERADE 2>/dev/null||true; }
trap cleanup EXIT; cleanup
ip netns add $NS
ip link add t4egr-h type veth peer name t4egr-p
ip link set t4egr-p netns $NS
ip addr add 10.231.0.1/30 dev t4egr-h; ip link set t4egr-h up
ip netns exec $NS ip addr add 10.231.0.2/30 dev t4egr-p
ip netns exec $NS ip link set t4egr-p up; ip netns exec $NS ip link set lo up
ip netns exec $NS ip route add default via 10.231.0.1
sysctl -qw net.ipv4.ip_forward=1
iptables -t nat -C POSTROUTING -s 10.231.0.0/30 -o $WAN -j MASQUERADE 2>/dev/null || iptables -t nat -A POSTROUTING -s 10.231.0.0/30 -o $WAN -j MASQUERADE
probe(){ ip netns exec $NS timeout 4 curl -s -o /dev/null -w "$1=%{http_code} " "$2" 2>/dev/null || echo -n "$1=timeout "; }
echo "== no policy =="; probe meta http://169.254.169.254/latest/meta-data/; probe cf https://1.1.1.1; echo
echo "== metadata-floor policy (default allow, metadata/host/private dropped) =="
~/dv/bin/python -c "import egress;print(egress.policy_nft_ruleset(egress.resolve('metadata-floor')))" | ip netns exec $NS nft -f -
probe meta http://169.254.169.254/latest/meta-data/; probe cf https://1.1.1.1; echo
echo "== deny-default + allow 1.1.1.1:443 only =="
ip netns exec $NS nft flush ruleset
~/dv/bin/python -c "import egress;print(egress.policy_nft_ruleset({'default':'deny','allow':[{'cidr':'1.1.1.1/32','proto':'tcp','port':443}]}))" | ip netns exec $NS nft -f -
probe meta http://169.254.169.254/latest/meta-data/; probe cf https://1.1.1.1; probe other https://9.9.9.9; echo
echo "== egress closed (deny all) =="
ip netns exec $NS nft flush ruleset
~/dv/bin/python -c "import egress;print(egress.policy_nft_ruleset(egress.resolve('none')))" | ip netns exec $NS nft -f -
probe meta http://169.254.169.254/latest/meta-data/; probe cf https://1.1.1.1; echo
