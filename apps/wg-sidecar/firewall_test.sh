#!/bin/sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
FIREWALL="$HERE/firewall.sh"
PASS=0
FAIL=0

ok() { PASS=$((PASS + 1)); }
bad() {
    FAIL=$((FAIL + 1))
    echo "FAIL: $1"
}

contains() {
    case "$1" in *"$2"*) ok ;; *) bad "$3: expected to contain [$2], got:
$1" ;; esac
}

lacks() {
    case "$1" in *"$2"*) bad "$3: must not contain [$2], got:
$1" ;; *) ok ;; esac
}

render() { PUBLIC_PORTS="$1" sh "$FIREWALL" render 2>&1; }

rejects() {
    if out=$(PUBLIC_PORTS="$1" sh "$FIREWALL" render 2>&1); then
        bad "PUBLIC_PORTS='$1' must be refused, got:
$out"
    else
        ok
    fi
}

with_nft() {
    SANDBOX=$(mktemp -d)
    cat >"$SANDBOX/nft" <<STUB
#!/bin/sh
if [ "\$1" = "-f" ]; then cat > "$SANDBOX/applied"; fi
exit $1
STUB
    chmod +x "$SANDBOX/nft"
}

out=$(render "")
contains "$out" 'iifname "wg0" tcp dport { 80, 443 } accept' "only the gateway's web ports are open by default"
contains "$out" 'iifname "wg0" drop' "everything else arriving on the tunnel is dropped"
contains "$out" 'iifname "wg0" ct state established,related accept' "replies to the pod's own connections still arrive"
contains "$out" 'iifname "wg0" meta l4proto ipv6-icmp accept' "ICMPv6 stays open so the tunnel probe and path MTU work"
lacks "$out" "udp dport" "no UDP port is open unless a chart declares one"
lacks "$out" "8888" "an app port is never open just because the app listens on it"

out=$(render "tcp/25565 udp/2456-2458 udp/9987")
contains "$out" 'tcp dport { 80, 443, 25565 } accept' "declared TCP ports join the web ports"
contains "$out" 'udp dport { 2456-2458, 9987 } accept' "declared UDP ports and ranges are opened"

drop_line=$(render "tcp/25565" | grep -n 'iifname "wg0" drop' | cut -d: -f1)
accept_line=$(render "tcp/25565" | grep -n 'tcp dport' | cut -d: -f1)
if [ "$accept_line" -lt "$drop_line" ]; then ok; else bad "the drop must come after every accept"; fi

out=$(render "")
contains "$out" "delete table inet yolab_tunnel" "a restart replaces the table instead of stacking a second copy"

rejects "25565"
rejects "sctp/25565"
rejects "tcp/"
rejects "tcp/abc"
rejects "tcp/0"
rejects "tcp/70000"
rejects "udp/3000-2000"
rejects "udp/1-2-3"
rejects "tcp/-5"
rejects "tcp/80;drop"

with_nft 0
if out=$(PATH="$SANDBOX:$PATH" PUBLIC_PORTS="tcp/8776" sh "$FIREWALL" 2>&1); then ok; else bad "applying a valid ruleset must succeed: $out"; fi
contains "$(cat "$SANDBOX/applied")" 'tcp dport { 80, 443, 8776 } accept' "the rendered ruleset is what reaches nft"

with_nft 1
if PATH="$SANDBOX:$PATH" PUBLIC_PORTS="" sh "$FIREWALL" >/dev/null 2>&1; then
    bad "a ruleset nft refuses must stop the sidecar before the tunnel comes up"
else
    ok
fi

with_nft 0
if PATH="$SANDBOX:$PATH" PUBLIC_PORTS="tcp/nope" sh "$FIREWALL" >/dev/null 2>&1; then
    bad "a bad PUBLIC_PORTS must stop the sidecar instead of opening nothing or everything"
else
    ok
fi
if [ -f "$SANDBOX/applied" ]; then bad "nothing may be applied when PUBLIC_PORTS is invalid"; else ok; fi

first_firewall=$(grep -n '/firewall.sh' "$HERE/entrypoint.sh" | cut -d: -f1)
first_up=$(grep -n 'wg-quick up' "$HERE/entrypoint.sh" | cut -d: -f1)
if [ -n "$first_firewall" ] && [ "$first_firewall" -lt "$first_up" ]; then
    ok
else
    bad "the firewall must be in place before wg-quick brings the public address up"
fi

echo "wg-sidecar firewall: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
