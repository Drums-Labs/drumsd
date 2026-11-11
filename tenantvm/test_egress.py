import egress as E

def test_deny_default_drops_everything_but_lo():
    r = E.policy_nft_ruleset({"default": "deny", "allow": []})
    assert 'oifname "lo" accept' in r
    assert "@never4 drop" in r and "@never6 drop" in r
    assert "counter drop" in r
    assert "169.254.0.0/16" in r

def test_allowlist_opens_only_listed_dest():
    r = E.policy_nft_ruleset({"default": "deny", "allow": [{"cidr": "1.2.3.0/24", "proto": "tcp", "port": 443}]})
    assert "ip daddr 1.2.3.0/24 tcp dport 443 accept" in r
    assert "counter drop" in r
    # the accept comes AFTER the never-drop, so metadata cannot be re-opened
    assert r.index("@never4 drop") < r.index("1.2.3.0/24")

def test_never_set_precedes_any_allow_even_if_allow_includes_metadata():
    r = E.policy_nft_ruleset({"default": "deny", "allow": [{"cidr": "169.254.169.254/32", "proto": "tcp", "port": 80}]})
    assert r.index("@never4 drop") < r.index("169.254.169.254/32")

def test_blocklist_floor_still_drops_metadata():
    r = E.policy_nft_ruleset({"default": "allow"})
    assert "@never4 drop" in r
    assert r.rstrip().endswith("}")

def test_ipv6_blocked():
    r = E.policy_nft_ruleset({"default": "allow"})
    assert "meta nfproto ipv6 drop" in r

def test_validate_rejects_bad():
    for bad in [{"default": "x"}, {"allow": [{"proto": "tcp"}]}, {"allow": [{"cidr": "x", "port": 0}]}]:
        try:
            E.policy_nft_ruleset(bad); assert False
        except ValueError:
            pass

def test_resolve_named():
    assert E.resolve("none")["default"] == "deny"
    assert E.resolve("metadata-floor")["default"] == "allow"
    try:
        E.resolve("bogus"); assert False
    except ValueError:
        pass
