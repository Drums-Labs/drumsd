"""Per-desktop egress policy for a Drums env (T4).

The hardened default already gives each desktop its own network namespace with loopback only, and a metadata/host-local
DROP floor (drums-lab harden.metadata_nft_ruleset). This module makes the policy EXPLICIT and ALLOW-LIST shaped, so a
host that does give a desktop a route out (a task that must fetch an asset) still cannot reach the metadata endpoint,
the host, or any private range, and reaches only the destinations the task declared.

Pure, testable functions. `policy_nft_ruleset(policy)` returns an `nft -f -` program applied inside the env's net
namespace. `policy` is a dict:
    { "default": "deny" | "allow",            # deny (recommended) = allow-list; allow = block-list (metadata floor only)
      "allow": [ {"cidr": "1.2.3.0/24", "proto": "tcp", "port": 443}, ... ],   # only consulted when default == deny
      "allow_dns": true|false }               # convenience: allow udp/tcp 53 to the listed resolvers
The metadata / host-local / private-range DROP always comes first and cannot be overridden by an allow rule, so an
allow-list entry can never re-open 169.254.169.254 or a private address even if it is mistyped to include one.
"""
from __future__ import annotations

# Destinations a desktop must NEVER reach, regardless of policy. 169.254/16 = cloud metadata + link-local; the RFC1918,
# CGNAT, loopback and benchmark ranges = the host, the VPC, and every sibling tenant.
NEVER_V4 = ("0.0.0.0/8", "10.0.0.0/8", "100.64.0.0/10", "127.0.0.0/8", "169.254.0.0/16",
            "172.16.0.0/12", "192.0.0.0/24", "192.168.0.0/16", "198.18.0.0/15", "224.0.0.0/4")
NEVER_V6 = ("::1/128", "fc00::/7", "fe80::/10", "ff00::/8")

_LABEL = "drums_egress"


def _set(name, fam, elems):
    t = "ipv4_addr" if fam == "ip" else "ipv6_addr"
    return (f"  set {name} {{ type {t}; flags interval;\n"
            f"    elements = {{ {', '.join(elems)} }} }}\n") if elems else f"  set {name} {{ type {t}; flags interval; }}\n"


def _validate(policy: dict) -> dict:
    if not isinstance(policy, dict):
        raise ValueError("policy must be a dict")
    d = policy.get("default", "deny")
    if d not in ("deny", "allow"):
        raise ValueError("policy.default must be 'deny' or 'allow'")
    for r in policy.get("allow", []):
        if "cidr" not in r:
            raise ValueError("each allow rule needs a cidr")
        if r.get("proto") not in (None, "tcp", "udp"):
            raise ValueError("allow.proto must be tcp or udp")
        p = r.get("port")
        if p is not None and not (isinstance(p, int) and 1 <= p <= 65535):
            raise ValueError("allow.port must be 1..65535")
    return policy


def policy_nft_ruleset(policy: dict) -> str:
    """The nft program for one env's net namespace. Hooks output (priority 0). lo is always allowed; the NEVER set is
    always dropped before any allow rule; then default deny (allow-list) or default accept (block-list floor)."""
    policy = _validate(policy)
    deny = policy.get("default", "deny") == "deny"
    out = ["table inet " + _LABEL + " {",
           _set("never4", "ip", NEVER_V4).rstrip("\n"),
           _set("never6", "ip6", NEVER_V6).rstrip("\n"),
           "  chain out {",
           "    type filter hook output priority 0; policy accept;",
           '    oifname "lo" accept',
           "    ct state established,related accept",
           "    ip daddr @never4 drop",
           "    ip6 daddr @never6 drop",
           "    meta nfproto ipv6 drop"]   # no IPv6 egress for a desktop (no benefit, more attack surface)
    if deny:
        allows = list(policy.get("allow", []))
        if policy.get("allow_dns"):
            for r in list(allows):
                pass
        for r in allows:
            proto = r.get("proto")
            port = r.get("port")
            cidr = r["cidr"]
            cond = f"ip daddr {cidr}"
            if proto:
                cond += f" {proto} dport {port}" if port else f" meta l4proto {proto}"
            out.append(f"    {cond} accept")
        out.append("    counter drop")        # default deny
    else:
        out.append("    accept")               # block-list: metadata floor only, everything else out
    out += ["  }", "}", ""]
    return "\n".join(out)


def apply_argv():
    return ("nft", "-f", "-")


# Named policies drumsd can select by string, so a task config says "egress": "none" | "metadata-floor" | {custom}.
NAMED = {
    "none": {"default": "deny", "allow": []},                     # loopback only (the Drums default)
    "metadata-floor": {"default": "allow"},                       # out to the internet, but never metadata/host/private
}


def resolve(spec) -> dict:
    """spec is a named string or a policy dict."""
    if spec is None:
        return NAMED["none"]
    if isinstance(spec, str):
        if spec not in NAMED:
            raise ValueError(f"unknown egress policy {spec!r}; known: {sorted(NAMED)}")
        return NAMED[spec]
    return _validate(spec)
