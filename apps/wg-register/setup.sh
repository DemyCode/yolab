#!/bin/sh
set -e

PLATFORM_API_URL="${PLATFORM_API_URL:?PLATFORM_API_URL is required}"
ACCOUNT_TOKEN="${ACCOUNT_TOKEN:?ACCOUNT_TOKEN is required}"
SERVICE_NAME="${SERVICE_NAME:-}"

WG_DIR="${WG_DIR:-/wireguard}"
YOLAB_DIR="${YOLAB_DIR:-/yolab}"
STATE_FILE="${STATE_FILE:-/state/wg-state.json}"

mkdir -p "$WG_DIR" "$YOLAB_DIR" "$(dirname "$STATE_FILE")"

OWNER="${POD_NAMESPACE:-}"
HANDSHAKE_FRESH_SECS="${HANDSHAKE_FRESH_SECS:-300}"
HANDSHAKE_WATCH_SECS="${HANDSHAKE_WATCH_SECS:-240}"
HANDSHAKE_POLL_SECS="${HANDSHAKE_POLL_SECS:-15}"
[ "$HANDSHAKE_POLL_SECS" -ge 1 ] || HANDSHAKE_POLL_SECS=1

REUSE=0
STATE_OWNER=""
if [ -f "$STATE_FILE" ]; then
    echo "Found existing state, attempting to reuse tunnel..."
    TUNNEL_ID=$(jq -r '.tunnel_id // empty' "$STATE_FILE")
    SUB_IPV6=$(jq -r '.sub_ipv6 // empty' "$STATE_FILE")
    PRIVATE_KEY=$(jq -r '.wg_private_key // empty' "$STATE_FILE")
    WG_SERVER_ENDPOINT=$(jq -r '.wg_server_endpoint // empty' "$STATE_FILE")
    WG_SERVER_PUBLIC_KEY=$(jq -r '.wg_server_public_key // empty' "$STATE_FILE")
    FQDN=$(jq -r '.fqdn // empty' "$STATE_FILE")
    STATE_OWNER=$(jq -r '.owner // empty' "$STATE_FILE")

    if [ -n "$TUNNEL_ID" ] && [ -n "$SUB_IPV6" ] && [ -n "$PRIVATE_KEY" ]; then
        echo "Reusing tunnel $TUNNEL_ID (IPv6: $SUB_IPV6)"
        REUSE=1
    else
        echo "State incomplete, re-registering..."
    fi
fi

fetch_tunnel() {
    TUNNEL_RESP=$(curl -s -w "\n%{http_code}" --max-time 10 \
        -H "Authorization: Bearer $ACCOUNT_TOKEN" \
        "$PLATFORM_API_URL/tunnels/$TUNNEL_ID" || true)
    VERIFY_HTTP=$(printf '%s' "$TUNNEL_RESP" | tail -1)
    VERIFY_BODY=$(printf '%s' "$TUNNEL_RESP" | head -n -1)
}

tunnel_field() {
    printf '%s' "$VERIFY_BODY" | jq -r ".$1 // empty" 2>/dev/null || true
}

copied_from_another_instance() {
    [ -n "$OWNER" ] && [ "$STATE_OWNER" != "$OWNER" ]
}

holder_is_live() {
    AGE=$(tunnel_field last_handshake_age_secs)
    [ -n "$AGE" ] || return 1
    [ "$AGE" -lt "$HANDSHAKE_FRESH_SECS" ] || return 1
    [ -z "$STATE_OWNER" ] || return 0
    echo "State predates ownership and the tunnel handshook ${AGE}s ago; watching whether that was another instance or this one before it restarted..."
    SEEN=$(tunnel_field last_handshake)
    WAITED=0
    while [ "$WAITED" -lt "$HANDSHAKE_WATCH_SECS" ]; do
        sleep "$HANDSHAKE_POLL_SECS"
        WAITED=$((WAITED + HANDSHAKE_POLL_SECS))
        fetch_tunnel
        [ "$VERIFY_HTTP" = "200" ] || continue
        LATEST=$(tunnel_field last_handshake)
        if [ -n "$LATEST" ] && [ "$LATEST" != "$SEEN" ]; then
            return 0
        fi
    done
    return 1
}

take_over() {
    NEW_PRIVATE_KEY=$(wg genkey)
    NEW_PUBLIC_KEY=$(printf '%s' "$NEW_PRIVATE_KEY" | wg pubkey)
    ROTATE_RESP=$(curl -s -w "\n%{http_code}" --max-time 10 \
        -X PUT "$PLATFORM_API_URL/tunnels/$TUNNEL_ID/key" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer $ACCOUNT_TOKEN" \
        -d "{\"wg_public_key\":\"$NEW_PUBLIC_KEY\",\"replaces\":\"$MY_PUBLIC_KEY\"}" || true)
    ROTATE_HTTP=$(printf '%s' "$ROTATE_RESP" | tail -1)
    ROTATE_BODY=$(printf '%s' "$ROTATE_RESP" | head -n -1)
    if [ "$ROTATE_HTTP" -ge 200 ] 2>/dev/null && [ "$ROTATE_HTTP" -lt 300 ]; then
        PRIVATE_KEY="$NEW_PRIVATE_KEY"
        echo "Took over tunnel $TUNNEL_ID with a fresh key; IPv6 $SUB_IPV6 is kept."
    elif [ "$ROTATE_HTTP" = "409" ] || [ "$ROTATE_HTTP" = "404" ]; then
        echo "Tunnel $TUNNEL_ID was claimed by another instance first (HTTP $ROTATE_HTTP), registering a new one..."
        REUSE=0
    else
        echo "ERROR: PUT /tunnels/$TUNNEL_ID/key returned HTTP $ROTATE_HTTP: $ROTATE_BODY" >&2
        exit 1
    fi
}

if [ "$REUSE" = "1" ]; then
    MY_PUBLIC_KEY=$(printf '%s' "$PRIVATE_KEY" | wg pubkey)
    fetch_tunnel
    if [ "$VERIFY_HTTP" = "404" ]; then
        echo "Tunnel $TUNNEL_ID was deleted on the platform, re-registering..."
        rm -f "$STATE_FILE"
        REUSE=0
    elif [ "$VERIFY_HTTP" = "200" ]; then
        PLATFORM_KEY=$(tunnel_field wg_public_key)
        if [ -n "$PLATFORM_KEY" ] && [ "$PLATFORM_KEY" != "$MY_PUBLIC_KEY" ]; then
            echo "Tunnel $TUNNEL_ID now answers to another key (taken over elsewhere), registering a new one..."
            REUSE=0
        elif ! copied_from_another_instance; then
            echo "Tunnel $TUNNEL_ID verified on platform."
        elif [ -z "$PLATFORM_KEY" ] && [ -z "$STATE_OWNER" ]; then
            echo "Tunnel $TUNNEL_ID verified on platform (it cannot report liveness, so this state is trusted as this instance's)."
        elif [ -z "$PLATFORM_KEY" ]; then
            echo "State was copied from $STATE_OWNER and the platform cannot say whether it is live, registering a new tunnel..."
            REUSE=0
        elif holder_is_live; then
            echo "Tunnel $TUNNEL_ID is live in ${STATE_OWNER:-another instance}; this is a copy, registering its own tunnel..."
            REUSE=0
        else
            echo "Tunnel $TUNNEL_ID was copied from ${STATE_OWNER:-an older instance} that is no longer live, taking it over..."
            take_over
        fi
    elif copied_from_another_instance && [ -n "$STATE_OWNER" ]; then
        echo "ERROR: state was copied from $STATE_OWNER and the platform returned HTTP $VERIFY_HTTP, so it cannot tell whether that instance is still live; refusing to share its key" >&2
        exit 1
    else
        echo "Platform returned HTTP $VERIFY_HTTP (unreachable or error), reusing cached state to stay online."
    fi
fi

if [ "$REUSE" = "1" ]; then
    TMP_STATE=$(mktemp)
    jq --arg owner "$OWNER" --arg key "$PRIVATE_KEY" \
        '.owner = $owner | .wg_private_key = $key' "$STATE_FILE" >"$TMP_STATE" && mv "$TMP_STATE" "$STATE_FILE"
    chmod 600 "$STATE_FILE"
fi

if [ "$REUSE" = "1" ] && [ -n "$SERVICE_NAME" ]; then
    echo "Re-asserting DNS record '$SERVICE_NAME' -> $SUB_IPV6..."
    REASSERT_RESP=$(curl -s -w "\n%{http_code}" --max-time 10 \
        -X POST "$PLATFORM_API_URL/tunnels/$TUNNEL_ID/records" \
        -H "Content-Type: application/json" \
        -H "Authorization: Bearer $ACCOUNT_TOKEN" \
        -d "{\"record_type\":\"AAAA\",\"name\":\"$SERVICE_NAME\",\"value\":\"$SUB_IPV6\"}")
    REASSERT_HTTP=$(printf '%s' "$REASSERT_RESP" | tail -1)
    REASSERT_BODY=$(printf '%s' "$REASSERT_RESP" | head -n -1)
    if [ "$REASSERT_HTTP" -ge 200 ] && [ "$REASSERT_HTTP" -lt 300 ]; then
        FQDN=$(printf '%s' "$REASSERT_BODY" | jq -r .fqdn)
        TMP_STATE=$(mktemp)
        jq --arg fqdn "$FQDN" '.fqdn = $fqdn' "$STATE_FILE" >"$TMP_STATE" && mv "$TMP_STATE" "$STATE_FILE"
        echo "DNS record re-asserted: $FQDN -> $SUB_IPV6"
    else
        echo "WARNING: DNS re-assert returned HTTP $REASSERT_HTTP: $REASSERT_BODY (continuing with cached state)"
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
            curl -s -o /dev/null --max-time 10 -X DELETE \
                -H "Authorization: Bearer $ACCOUNT_TOKEN" \
                "$PLATFORM_API_URL/tunnels/$TUNNEL_ID" || true
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
        --arg owner "$OWNER" \
        '{tunnel_id: $tunnel_id, sub_ipv6: $sub_ipv6, wg_private_key: $wg_private_key,
          wg_server_endpoint: $wg_server_endpoint, wg_server_public_key: $wg_server_public_key,
          fqdn: $fqdn, owner: $owner}' >"$STATE_FILE"
    chmod 600 "$STATE_FILE"
fi

URL=""
[ -n "$FQDN" ] && URL="https://$FQDN"

cat >"$WG_DIR/wg0.conf" <<EOF
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

cat >"$YOLAB_DIR/env" <<EOF
export YOLAB_TUNNEL_ID=$TUNNEL_ID
export YOLAB_IPV6=$SUB_IPV6
export YOLAB_FQDN=$FQDN
export YOLAB_URL=$URL
EOF

echo "YOLAB_OUTPUT tunnel_id $TUNNEL_ID"
echo "YOLAB_OUTPUT ipv6 $SUB_IPV6"
[ -n "$URL" ] && echo "YOLAB_OUTPUT url $URL"

echo "Done. Tunnel: $TUNNEL_ID  IPv6: $SUB_IPV6${FQDN:+  FQDN: $FQDN}"
