#!/bin/sh
set -e

PLATFORM_API_URL="${PLATFORM_API_URL:?PLATFORM_API_URL is required}"
ACCOUNT_TOKEN="${ACCOUNT_TOKEN:?ACCOUNT_TOKEN is required}"
SERVICE_NAME="${SERVICE_NAME:-}"

WG_DIR="/wireguard"
YOLAB_DIR="/yolab"
STATE_FILE="/state/wg-state.json"

mkdir -p "$WG_DIR" "$YOLAB_DIR" "$(dirname "$STATE_FILE")"

REUSE=0
if [ -f "$STATE_FILE" ]; then
    echo "Found existing state, attempting to reuse tunnel..."
    TUNNEL_ID=$(jq -r '.tunnel_id // empty' "$STATE_FILE")
    SUB_IPV6=$(jq -r '.sub_ipv6 // empty' "$STATE_FILE")
    PRIVATE_KEY=$(jq -r '.wg_private_key // empty' "$STATE_FILE")
    WG_SERVER_ENDPOINT=$(jq -r '.wg_server_endpoint // empty' "$STATE_FILE")
    WG_SERVER_PUBLIC_KEY=$(jq -r '.wg_server_public_key // empty' "$STATE_FILE")
    FQDN=$(jq -r '.fqdn // empty' "$STATE_FILE")

    if [ -n "$TUNNEL_ID" ] && [ -n "$SUB_IPV6" ] && [ -n "$PRIVATE_KEY" ]; then
        echo "Reusing tunnel $TUNNEL_ID (IPv6: $SUB_IPV6)"
        REUSE=1
    else
        echo "State incomplete, re-registering..."
    fi
fi

if [ "$REUSE" = "0" ]; then
    echo "Generating WireGuard keypair..."
    PRIVATE_KEY=$(wg genkey)
    PUBLIC_KEY=$(printf '%s' "$PRIVATE_KEY" | wg pubkey)

    echo "Registering tunnel..."
    TUNNEL_RESP=$(curl -s -w "\n%{http_code}" -X POST "$PLATFORM_API_URL/tunnels" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer $ACCOUNT_TOKEN" \
        -d "{\"wg_public_key\":\"$PUBLIC_KEY\"}")
    TUNNEL_HTTP=$(printf '%s' "$TUNNEL_RESP" | tail -1)
    TUNNEL_BODY=$(printf '%s' "$TUNNEL_RESP" | head -n -1)
    if [ "$TUNNEL_HTTP" -lt 200 ] || [ "$TUNNEL_HTTP" -ge 300 ]; then
        echo "ERROR: POST /tunnels returned HTTP $TUNNEL_HTTP: $TUNNEL_BODY" >&2
        exit 1
    fi

    TUNNEL_ID=$(printf '%s' "$TUNNEL_BODY" | jq -r .tunnel_id)
    SUB_IPV6=$(printf '%s' "$TUNNEL_BODY" | jq -r .sub_ipv6)
    WG_SERVER_ENDPOINT=$(printf '%s' "$TUNNEL_BODY" | jq -r .wg_server_endpoint)
    WG_SERVER_PUBLIC_KEY=$(printf '%s' "$TUNNEL_BODY" | jq -r .wg_server_public_key)
    FQDN=""

    if [ -n "$SERVICE_NAME" ]; then
        echo "Creating DNS record '$SERVICE_NAME'..."
        RECORD_RESP=$(curl -s -w "\n%{http_code}" -X POST "$PLATFORM_API_URL/tunnels/$TUNNEL_ID/records" \
            -H "Content-Type: application/json" \
            -H "Authorization: Bearer $ACCOUNT_TOKEN" \
            -d "{\"record_type\":\"AAAA\",\"name\":\"$SERVICE_NAME\",\"value\":\"$SUB_IPV6\"}")
        RECORD_HTTP=$(printf '%s' "$RECORD_RESP" | tail -1)
        RECORD_BODY=$(printf '%s' "$RECORD_RESP" | head -n -1)
        if [ "$RECORD_HTTP" -lt 200 ] || [ "$RECORD_HTTP" -ge 300 ]; then
            echo "ERROR: POST /tunnels/$TUNNEL_ID/records returned HTTP $RECORD_HTTP: $RECORD_BODY" >&2
            exit 1
        fi
        FQDN=$(printf '%s' "$RECORD_BODY" | jq -r .fqdn)
    fi

    jq -n \
        --argjson tunnel_id "$TUNNEL_ID" \
        --arg sub_ipv6 "$SUB_IPV6" \
        --arg wg_private_key "$PRIVATE_KEY" \
        --arg wg_server_endpoint "$WG_SERVER_ENDPOINT" \
        --arg wg_server_public_key "$WG_SERVER_PUBLIC_KEY" \
        --arg fqdn "$FQDN" \
        '{tunnel_id: $tunnel_id, sub_ipv6: $sub_ipv6, wg_private_key: $wg_private_key,
          wg_server_endpoint: $wg_server_endpoint, wg_server_public_key: $wg_server_public_key,
          fqdn: $fqdn}' > "$STATE_FILE"
    chmod 600 "$STATE_FILE"
fi

URL=""
[ -n "$FQDN" ] && URL="https://$FQDN"

cat > "$WG_DIR/wg0.conf" << EOF
[Interface]
PrivateKey = $PRIVATE_KEY
Table = off
PostUp = ip -6 address add $SUB_IPV6/128 dev wg0 || true; ip -6 rule add from $SUB_IPV6 lookup 51820 priority 100 || true; ip -6 route add ::/0 dev wg0 table 51820 || true
PreDown = ip -6 rule del from $SUB_IPV6 lookup 51820 priority 100 || true; ip -6 route del ::/0 dev wg0 table 51820 || true; ip -6 address del $SUB_IPV6/128 dev wg0 || true

[Peer]
PublicKey = $WG_SERVER_PUBLIC_KEY
Endpoint = $WG_SERVER_ENDPOINT
AllowedIPs = ::/0
PersistentKeepalive = 25
EOF
chmod 600 "$WG_DIR/wg0.conf"

cat > "$YOLAB_DIR/env" << EOF
export YOLAB_TUNNEL_ID=$TUNNEL_ID
export YOLAB_IPV6=$SUB_IPV6
export YOLAB_FQDN=$FQDN
export YOLAB_URL=$URL
EOF

echo "YOLAB_OUTPUT tunnel_id $TUNNEL_ID"
echo "YOLAB_OUTPUT ipv6 $SUB_IPV6"
[ -n "$URL" ] && echo "YOLAB_OUTPUT url $URL"

echo "Done. Tunnel: $TUNNEL_ID  IPv6: $SUB_IPV6${FQDN:+  FQDN: $FQDN}"
