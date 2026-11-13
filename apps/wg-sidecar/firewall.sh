#!/bin/sh
set -eu

render() {
    tcp="80, 443"
    udp=""
    for entry in ${PUBLIC_PORTS:-}; do
        case "$entry" in
        tcp/* | udp/*) ;;
        *)
            echo "PUBLIC_PORTS entry '$entry' is not tcp/PORT or udp/PORT[-PORT]" >&2
            return 1
            ;;
        esac
        proto=${entry%%/*}
        ports=${entry#*/}
        case "$ports" in
        "" | *[!0-9-]* | -* | *- | *-*-*)
            echo "PUBLIC_PORTS entry '$entry' has no valid port or range" >&2
            return 1
            ;;
        esac
        low=${ports%-*}
        high=${ports#*-}
        for p in "$low" "$high"; do
            if [ "$p" -lt 1 ] || [ "$p" -gt 65535 ]; then
                echo "PUBLIC_PORTS entry '$entry' is outside 1-65535" >&2
                return 1
            fi
        done
        if [ "$low" -gt "$high" ]; then
            echo "PUBLIC_PORTS entry '$entry' is a reversed range" >&2
            return 1
        fi
        if [ "$proto" = tcp ]; then
            tcp="$tcp, $ports"
        elif [ -z "$udp" ]; then
            udp="$ports"
        else
            udp="$udp, $ports"
        fi
    done

    echo "table inet yolab_tunnel"
    echo "delete table inet yolab_tunnel"
    echo "table inet yolab_tunnel {"
    echo "    chain input {"
    echo "        type filter hook input priority filter; policy accept;"
    echo "        iifname \"wg0\" ct state established,related accept"
    echo "        iifname \"wg0\" meta l4proto ipv6-icmp accept"
    echo "        iifname \"wg0\" tcp dport { $tcp } accept"
    if [ -n "$udp" ]; then
        echo "        iifname \"wg0\" udp dport { $udp } accept"
    fi
    echo "        iifname \"wg0\" drop"
    echo "    }"
    echo "}"
}

if [ "${1:-}" = "render" ]; then
    render
    exit 0
fi

ruleset=$(render)
printf '%s\n' "$ruleset" | nft -f -
nft list table inet yolab_tunnel >/dev/null
echo "tunnel firewall: inbound on wg0 limited to tcp 80, 443${PUBLIC_PORTS:+ and $PUBLIC_PORTS}"
