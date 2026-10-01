#!/bin/sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
SETUP="$HERE/setup.sh"
PASS=0
FAIL=0

new_sandbox() {
    SANDBOX=$(mktemp -d)
    export SANDBOX
    mkdir -p "$SANDBOX/bin" "$SANDBOX/state" "$SANDBOX/resp"

    cat >"$SANDBOX/bin/curl" <<'STUB'
#!/bin/sh
url=""; method="GET"; code_only=0; data=""
while [ $# -gt 0 ]; do
    case "$1" in
        -X) method="$2"; shift 2 ;;
        -o) [ "$2" = "/dev/null" ] && code_only=1; shift 2 ;;
        -d) data="$2"; shift 2 ;;
        -H|-w|--max-time) shift 2 ;;
        http*) url="$1"; shift ;;
        *) shift ;;
    esac
done
case "$method:$url" in
    GET:*/tunnels/*)     key=verify ;;
    POST:*/tunnels)      key=create ;;
    POST:*/records)      key=records ;;
    PUT:*/key)           key=rotate ;;
    *)                   key=unknown ;;
esac
echo "$method $key $data" >> "$SANDBOX/calls.log"
n=$(grep -c "^$method $key" "$SANDBOX/calls.log")
file="$SANDBOX/resp/$key.$n"
[ -f "$file" ] || file="$SANDBOX/resp/$key"
[ -f "$file" ] || { echo "000"; exit 0; }
http=$(head -1 "$file")
body=$(tail -n +2 "$file")
if [ "$code_only" = "1" ]; then
    printf '%s' "$http"
else
    printf '%s\n%s' "$body" "$http"
fi
STUB

    cat >"$SANDBOX/bin/wg" <<'STUB'
#!/bin/sh
case "$1" in
    genkey) echo "PRIVKEY-generated" ;;
    pubkey) read -r key; echo "PUB-$key" ;;
esac
STUB

    cat >"$SANDBOX/bin/sleep" <<'STUB'
#!/bin/sh
echo "sleep $1" >> "$SANDBOX/calls.log"
STUB

    chmod +x "$SANDBOX/bin/curl" "$SANDBOX/bin/wg" "$SANDBOX/bin/sleep"
    : >"$SANDBOX/calls.log"
}

respond() {
    printf '%s\n%s' "$2" "$3" >"$SANDBOX/resp/$1"
}

write_state() { cat >"$SANDBOX/state/wg-state.json"; }

run_setup() {
    OUT="$SANDBOX/output.txt"
    WG_DIR="$SANDBOX/wireguard" \
        YOLAB_DIR="$SANDBOX/yolab" \
        STATE_FILE="$SANDBOX/state/wg-state.json" \
        TERMINATION_LOG="$SANDBOX/termination-log" \
        PATH="$SANDBOX/bin:$PATH" \
        PLATFORM_API_URL="https://api.example.test" \
        ACCOUNT_TOKEN="test-token" \
        SERVICE_NAME="${SERVICE_NAME_OVERRIDE-myapp}" \
        ALIASES="${ALIASES_OVERRIDE-}" \
        POD_NAMESPACE="${OWNER_OVERRIDE-yolab-myapp-cd34}" \
        HANDSHAKE_POLL_SECS=1 \
        HANDSHAKE_WATCH_SECS=3 \
        sh "$SETUP" >"$OUT" 2>&1
    RC=$?
}

state() { cat "$SANDBOX/state/wg-state.json" 2>/dev/null; }
state_field() { jq -r ".$1 // empty" "$SANDBOX/state/wg-state.json" 2>/dev/null; }
wg_conf() { cat "$SANDBOX/wireguard/wg0.conf" 2>/dev/null; }
env_file() { cat "$SANDBOX/yolab/env" 2>/dev/null; }
called() { grep -q "$1" "$SANDBOX/calls.log"; }

ok() { PASS=$((PASS + 1)); }
bad() {
    FAIL=$((FAIL + 1))
    printf 'FAIL %s\n     %s\n' "$CASE" "$1"
}

assert_contains() {
    case "$1" in
    *"$2"*) ok ;;
    *) bad "$3: expected to contain '$2', got: $(printf '%s' "$1" | head -c 300)" ;;
    esac
}
assert_missing() {
    case "$1" in
    *"$2"*) bad "$3: expected NOT to contain '$2'" ;;
    *) ok ;;
    esac
}
assert_eq() {
    if [ "$1" = "$2" ]; then ok; else bad "$3: expected '$2', got '$1'"; fi
}
assert_called() { if called "$1"; then ok; else bad "$2: expected a $1 request"; fi; }
assert_not_called() { if called "$1"; then bad "$2: unexpected $1 request"; else ok; fi; }

case_start() {
    CASE="$1"
    new_sandbox
}
case_end() {
    rm -rf "$SANDBOX"
    unset SERVICE_NAME_OVERRIDE OWNER_OVERRIDE ALIASES_OVERRIDE
}

TUNNEL_BODY='{"tunnel_id":77,"sub_ipv6":"2001:db8::99","wg_server_endpoint":"1.2.3.4:51820","wg_server_public_key":"SERVER-PUB"}'
RECORD_BODY='{"fqdn":"myapp.example.test"}'

CACHED_STATE='{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_private_key":"PRIVKEY-cached",
 "wg_server_endpoint":"9.9.9.9:51820","wg_server_public_key":"CACHED-SERVER-PUB","fqdn":"old.example.test"}'

case_start "fresh install registers a tunnel and writes every artifact"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called "POST create" "registration"
assert_contains "$(state)" '"tunnel_id": 77' "state file"
assert_contains "$(state)" 'PRIVKEY-generated' "state file"
assert_contains "$(state)" 'myapp.example.test' "state file"
case_end

case_start "the generated private key reaches wg0.conf, the public key never does"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_contains "$(wg_conf)" 'PrivateKey = PRIVKEY-generated' "wg0.conf"
assert_contains "$(wg_conf)" 'PublicKey = SERVER-PUB' "wg0.conf peer"
assert_contains "$(wg_conf)" 'Endpoint = 1.2.3.4:51820' "wg0.conf peer"
assert_contains "$(wg_conf)" '2001:db8::99/128' "wg0.conf address"
case_end

case_start "the app tunnel carries only replies from its public address, never the app's own outbound traffic"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_contains "$(wg_conf)" 'Table = off' "wg-quick must not install a default route"
assert_contains "$(wg_conf)" 'ip -6 rule add from 2001:db8::99 lookup 51820' "only the public address is steered into the tunnel"
assert_missing "$(wg_conf | grep -o 'route add ::/0 dev wg0[^;]*' | grep -v 'table 51820')" '::/0' "a default route through the tunnel outside table 51820"
case_end

case_start "the env file exports what the app containers source"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_contains "$(env_file)" 'export YOLAB_FQDN=myapp.example.test' "env"
assert_contains "$(env_file)" 'export YOLAB_URL=https://myapp.example.test' "env"
assert_contains "$(env_file)" 'export YOLAB_IPV6=2001:db8::99' "env"
case_end

case_start "the state file holding the private key is owner-only"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$(stat -c %a "$SANDBOX/state/wg-state.json")" "600" "state permissions"
assert_eq "$(stat -c %a "$SANDBOX/wireguard/wg0.conf")" "600" "wg0.conf permissions"
case_end

case_start "an app with no DNS name gets no FQDN and no URL"
SERVICE_NAME_OVERRIDE=""
respond create 200 "$TUNNEL_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST records" "no service name"
assert_contains "$(env_file)" 'export YOLAB_FQDN=' "env"
assert_contains "$(env_file)" 'export YOLAB_URL=' "env"
assert_missing "$(env_file)" 'https://' "env should carry no URL"
case_end

case_start "a tunnel the platform still knows about is reused, not recreated"
write_state <<EOF
$CACHED_STATE
EOF
respond verify 200 '{}'
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST create" "reuse"
assert_contains "$(wg_conf)" 'PrivateKey = PRIVKEY-cached' "wg0.conf must use the cached key"
assert_contains "$(wg_conf)" '2001:db8::42/128' "wg0.conf must use the cached address"
case_end

case_start "reusing a tunnel re-asserts its DNS record"
write_state <<EOF
$CACHED_STATE
EOF
respond verify 200 '{}'
respond records 200 '{"fqdn":"myapp.example.test"}'
run_setup
assert_called "POST records" "DNS re-assert"
assert_contains "$(state)" 'myapp.example.test' "the refreshed FQDN must be persisted"
assert_missing "$(state)" 'old.example.test' "the stale FQDN must be replaced"
case_end

case_start "a failed DNS re-assert is not fatal"
write_state <<EOF
$CACHED_STATE
EOF
respond verify 200 '{}'
respond records 500 '{"detail":"boom"}'
run_setup
assert_eq "$RC" "0" "a DNS blip must not take the app down"
assert_contains "$(wg_conf)" 'PRIVKEY-cached' "the tunnel still comes up"
assert_contains "$(cat "$OUT")" 'WARNING' "the failure is reported"
case_end

case_start "a tunnel deleted on the platform is re-registered"
write_state <<EOF
$CACHED_STATE
EOF
respond verify 404 '{"detail":"not found"}'
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called "POST create" "re-registration"
assert_contains "$(state)" '"tunnel_id": 77' "state must hold the new tunnel"
assert_contains "$(wg_conf)" 'PRIVKEY-generated' "wg0.conf must use the new key"
case_end

case_start "state missing required fields is discarded rather than half-used"
write_state <<'EOF'
{"tunnel_id":42,"fqdn":"old.example.test"}
EOF
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "GET verify" "incomplete state must not be verified"
assert_called "POST create" "re-registration"
case_end

case_start "an unreachable platform does not cost the app its tunnel"
write_state <<EOF
$CACHED_STATE
EOF
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST create" "an outage must not trigger re-registration"
assert_contains "$(wg_conf)" 'PRIVKEY-cached' "the cached tunnel keeps serving"
assert_eq "$(state_field tunnel_id)" "42" "cached state must survive"
case_end

case_start "a platform 500 does not cost the app its tunnel either"
write_state <<EOF
$CACHED_STATE
EOF
respond verify 500 '{"detail":"internal error"}'
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST create" "a 5xx must not trigger re-registration"
assert_contains "$(wg_conf)" 'PRIVKEY-cached' "the cached tunnel keeps serving"
case_end

case_start "a 401 does not destroy state — only an explicit 404 does"
write_state <<EOF
$CACHED_STATE
EOF
respond verify 401 '{"detail":"unauthorized"}'
respond records 200 "$RECORD_BODY"
run_setup
assert_not_called "POST create" "a bad token must not wipe a working tunnel"
assert_eq "$(state_field tunnel_id)" "42" "cached state must survive"
case_end

case_start "a rejected tunnel registration fails the init container"
respond create 403 '{"detail":"quota exceeded"}'
run_setup
if [ "$RC" -ne 0 ]; then ok; else bad "expected a non-zero exit, got $RC"; fi
assert_contains "$(cat "$OUT")" 'ERROR' "the reason is reported"
assert_contains "$(cat "$OUT")" '403' "the status code is reported"
case_end

case_start "a rejected DNS record fails the init container"
respond create 200 "$TUNNEL_BODY"
respond records 409 '{"detail":"name taken"}'
run_setup
if [ "$RC" -ne 0 ]; then ok; else bad "expected a non-zero exit, got $RC"; fi
assert_contains "$(cat "$OUT")" 'ERROR' "the reason is reported"
case_end

case_start "a rejected DNS record leaves the platform's own sentence as the container's last word"
respond create 200 "$TUNNEL_BODY"
respond records 409 '{"detail":"myapp.example.test is already used by another app on this account"}'
run_setup
assert_eq "$(cat "$SANDBOX/termination-log" 2>/dev/null)" \
    "myapp.example.test is already used by another app on this account" \
    "the termination message is exactly what the platform said"
case_end

case_start "a rejection the platform did not explain still names the address it was for"
respond create 200 "$TUNNEL_BODY"
respond records 500 'upstream exploded'
run_setup
assert_eq "$(cat "$SANDBOX/termination-log" 2>/dev/null)" \
    "could not claim the web address 'myapp' (HTTP 500)" \
    "the termination message falls back to a sentence of our own"
case_end

case_start "a rejected tunnel registration leaves the platform's reason as the container's last word"
respond create 403 '{"detail":"quota exceeded"}'
run_setup
assert_eq "$(cat "$SANDBOX/termination-log" 2>/dev/null)" "quota exceeded" \
    "the termination message is what the platform said"
case_end

case_start "a successful registration leaves no termination message"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
if [ -e "$SANDBOX/termination-log" ]; then bad "nothing failed, yet a reason was written"; else ok; fi
case_end

case_start "a missing account token fails immediately"
OUT="$SANDBOX/output.txt"
WG_DIR="$SANDBOX/wireguard" YOLAB_DIR="$SANDBOX/yolab" PATH="$SANDBOX/bin:$PATH" \
    STATE_FILE="$SANDBOX/state/wg-state.json" PLATFORM_API_URL="https://api.example.test" \
    sh "$SETUP" >"$OUT" 2>&1
RC=$?
if [ "$RC" -ne 0 ]; then ok; else bad "expected a non-zero exit, got $RC"; fi
assert_not_called "POST create" "nothing should be requested without a token"
case_end

OWNED_STATE='{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_private_key":"PRIVKEY-cached",
 "wg_server_endpoint":"9.9.9.9:51820","wg_server_public_key":"CACHED-SERVER-PUB","fqdn":"old.example.test",
 "owner":"yolab-myapp-cd34"}'
COPIED_STATE='{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_private_key":"PRIVKEY-cached",
 "wg_server_endpoint":"9.9.9.9:51820","wg_server_public_key":"CACHED-SERVER-PUB","fqdn":"old.example.test",
 "owner":"yolab-myapp-ab12"}'
LIVE_TUNNEL='{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_public_key":"PUB-PRIVKEY-cached",
 "last_handshake":"2026-09-29T10:00:00Z","last_handshake_age_secs":20}'
DEAD_TUNNEL='{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_public_key":"PUB-PRIVKEY-cached",
 "last_handshake":"2026-09-01T10:00:00Z","last_handshake_age_secs":2419200}'
NEVER_SEEN_TUNNEL='{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_public_key":"PUB-PRIVKEY-cached",
 "last_handshake":null,"last_handshake_age_secs":null}'
LIVE_TUNNEL_LATER='{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_public_key":"PUB-PRIVKEY-cached",
 "last_handshake":"2026-09-29T10:02:00Z","last_handshake_age_secs":5}'

case_start "a fresh registration records which instance owns the tunnel"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$(state_field owner)" "yolab-myapp-cd34" "owner"
case_end

case_start "an instance restarting on its own live tunnel keeps it untouched"
write_state <<EOF
$OWNED_STATE
EOF
respond verify 200 "$LIVE_TUNNEL"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST create" "a restart must not re-register"
assert_not_called "PUT rotate" "a restart must not rotate its key"
assert_contains "$(wg_conf)" 'PrivateKey = PRIVKEY-cached' "wg0.conf"
case_end

case_start "a duplicate of a live instance registers its own address"
write_state <<EOF
$COPIED_STATE
EOF
respond verify 200 "$LIVE_TUNNEL"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called "POST create" "the copy needs its own tunnel"
assert_not_called "PUT rotate" "the live original must keep its key"
assert_contains "$(wg_conf)" '2001:db8::99/128' "wg0.conf must use the new address"
assert_missing "$(wg_conf)" 'PRIVKEY-cached' "the original's key must never be shared"
assert_eq "$(state_field tunnel_id)" "77" "state holds the new tunnel"
assert_eq "$(state_field owner)" "yolab-myapp-cd34" "state belongs to the copy"
case_end

case_start "a restore of an instance that is gone keeps its address under a fresh key"
write_state <<EOF
$COPIED_STATE
EOF
respond verify 200 "$DEAD_TUNNEL"
respond rotate 200 '{}'
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST create" "the address must be kept"
assert_called 'PUT rotate {"wg_public_key":"PUB-PRIVKEY-generated","replaces":"PUB-PRIVKEY-cached"}' "the key is swapped only if still the old one"
assert_contains "$(wg_conf)" '2001:db8::42/128' "wg0.conf keeps the address"
assert_contains "$(wg_conf)" 'PrivateKey = PRIVKEY-generated' "wg0.conf uses the fresh key"
assert_eq "$(state_field tunnel_id)" "42" "state keeps the tunnel"
assert_eq "$(state_field wg_private_key)" "PRIVKEY-generated" "state keeps the fresh key"
assert_eq "$(state_field owner)" "yolab-myapp-cd34" "state belongs to the restore"
case_end

case_start "a restore of a tunnel that never handshook takes it over"
write_state <<EOF
$COPIED_STATE
EOF
respond verify 200 "$NEVER_SEEN_TUNNEL"
respond rotate 200 '{}'
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST create" "the address must be kept"
assert_called "PUT rotate" "takeover"
case_end

case_start "a restore that loses the takeover race registers its own address"
write_state <<EOF
$COPIED_STATE
EOF
respond verify 200 "$DEAD_TUNNEL"
respond rotate 409 '{"detail":"key changed"}'
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called "POST create" "the loser needs its own tunnel"
assert_eq "$(state_field tunnel_id)" "77" "state holds the new tunnel"
case_end

case_start "a takeover the platform fails fails the init container instead of guessing"
write_state <<EOF
$COPIED_STATE
EOF
respond verify 200 "$DEAD_TUNNEL"
respond rotate 500 '{"detail":"boom"}'
run_setup
if [ "$RC" -ne 0 ]; then ok; else bad "expected a non-zero exit, got $RC"; fi
assert_not_called "POST create" "no new tunnel on an ambiguous failure"
assert_eq "$(state_field owner)" "yolab-myapp-ab12" "the copied state is left for the retry"
case_end

case_start "an instance whose tunnel was taken over elsewhere moves to a new address"
write_state <<EOF
$OWNED_STATE
EOF
respond verify 200 '{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_public_key":"PUB-someone-else","last_handshake_age_secs":3}'
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called "POST create" "the stale key must not be used"
assert_missing "$(wg_conf)" 'PRIVKEY-cached' "wg0.conf must not carry the replaced key"
case_end

case_start "a copy cannot share the key while the platform is unreachable"
write_state <<EOF
$COPIED_STATE
EOF
run_setup
if [ "$RC" -ne 0 ]; then ok; else bad "expected a non-zero exit, got $RC"; fi
assert_not_called "POST create" "nothing is registered blind"
assert_missing "$(wg_conf)" 'PRIVKEY-cached' "the copied key must not come up"
case_end

case_start "a copy on a platform that cannot report liveness registers its own address"
write_state <<EOF
$COPIED_STATE
EOF
respond verify 200 '{"tunnel_id":42,"sub_ipv6":"2001:db8::42"}'
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called "POST create" "unknown liveness must not share a key"
assert_not_called "PUT rotate" "no takeover without liveness"
case_end

case_start "pre-ownership state reused on a platform without liveness is adopted"
write_state <<EOF
$CACHED_STATE
EOF
respond verify 200 '{"tunnel_id":42,"sub_ipv6":"2001:db8::42"}'
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST create" "upgrading must not move every app to a new address"
assert_eq "$(state_field owner)" "yolab-myapp-cd34" "the state is claimed"
case_end

case_start "pre-ownership state whose tunnel keeps handshaking is another live instance"
write_state <<EOF
$CACHED_STATE
EOF
respond verify.1 200 "$LIVE_TUNNEL"
respond verify 200 "$LIVE_TUNNEL_LATER"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called "sleep" "it watches before deciding"
assert_called "POST create" "a duplicate of an older instance gets its own address"
assert_not_called "PUT rotate" "the live original keeps its key"
case_end

case_start "pre-ownership state whose tunnel went quiet was this instance before it restarted"
write_state <<EOF
$CACHED_STATE
EOF
respond verify 200 "$LIVE_TUNNEL"
respond rotate 200 '{}'
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called "sleep" "it watches before deciding"
assert_not_called "POST create" "the address must be kept"
assert_contains "$(wg_conf)" '2001:db8::42/128' "wg0.conf keeps the address"
assert_eq "$(state_field owner)" "yolab-myapp-cd34" "the state is claimed"
case_end

case_start "a chart that passes no namespace keeps today's reuse behaviour"
OWNER_OVERRIDE=""
write_state <<EOF
$COPIED_STATE
EOF
respond verify 200 "$LIVE_TUNNEL"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_not_called "POST create" "no ownership information, no decision"
case_end

ALIAS_BODY='{"fqdn":"myapp-files.example.test"}'

case_start "an alias is claimed on the same tunnel address and exported under its variable"
ALIASES_OVERRIDE="FILE_EXPLORER_FQDN=myapp-files"
respond create 200 "$TUNNEL_BODY"
respond records.1 200 "$RECORD_BODY"
respond records.2 200 "$ALIAS_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_called '"name":"myapp-files","value":"2001:db8::99"' "alias record points at the tunnel"
assert_contains "$(env_file)" 'export FILE_EXPLORER_FQDN=myapp-files.example.test' "env"
assert_contains "$(env_file)" 'export YOLAB_FQDN=myapp.example.test' "env keeps the app's own name"
assert_eq "$(state_field 'aliases.FILE_EXPLORER_FQDN')" "myapp-files.example.test" "alias cached"
case_end

case_start "an alias held by another app stops the pod with the platform's reason"
ALIASES_OVERRIDE="FILE_EXPLORER_FQDN=taken"
respond create 200 "$TUNNEL_BODY"
respond records.1 200 "$RECORD_BODY"
respond records.2 409 '{"error":"taken.example.test is already used by another app on this account"}'
run_setup
assert_eq "$RC" "1" "exit code"
assert_contains "$(cat "$OUT")" "already used by another app" "reason surfaced"
case_end

case_start "an unreachable platform keeps serving an alias claimed before"
ALIASES_OVERRIDE="FILE_EXPLORER_FQDN=myapp-files"
write_state <<'EOF'
{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_private_key":"PRIVKEY-cached",
 "wg_server_endpoint":"9.9.9.9:51820","wg_server_public_key":"CACHED-SERVER-PUB",
 "fqdn":"myapp.example.test","owner":"yolab-myapp-cd34",
 "aliases":{"FILE_EXPLORER_FQDN":"myapp-files.example.test"}}
EOF
run_setup
assert_eq "$RC" "0" "exit code"
assert_contains "$(env_file)" 'export FILE_EXPLORER_FQDN=myapp-files.example.test' "cached alias exported"
case_end

case_start "an unreachable platform cannot invent an alias it never claimed"
ALIASES_OVERRIDE="FILE_EXPLORER_FQDN=myapp-files"
write_state <<EOF
$CACHED_STATE
EOF
run_setup
assert_eq "$RC" "1" "exit code"
assert_contains "$(cat "$OUT")" "never claimed before" "reason surfaced"
case_end

case_start "no aliases means exactly one DNS record"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "0" "exit code"
assert_eq "$(grep -c '^POST records' "$SANDBOX/calls.log")" "1" "record calls"
case_end

case_start "a malformed alias is refused before anything is claimed for it"
ALIASES_OVERRIDE="file-explorer=myapp-files"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_eq "$RC" "1" "exit code"
assert_eq "$(grep -c '^POST records' "$SANDBOX/calls.log")" "1" "only the app's own record"
case_end

echo "wg-register: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
