#!/bin/sh
# Tests for setup.sh — the init container that runs on every app install and on
# every restart of every app. It decides whether to reuse a cached tunnel or
# register a new one, and getting that wrong is not a failed install: it is an
# app that silently loses its public address, or one that goes offline during a
# platform outage it should have ridden out.
#
# `curl` and `wg` are replaced with stubs on PATH, so no network and no kernel
# module are involved. Everything else — jq, the shell, the file writing — is
# the real thing.
#
# Run:  sh apps/wg-register/setup_test.sh
set -u

HERE=$(cd "$(dirname "$0")" && pwd)
SETUP="$HERE/setup.sh"
PASS=0
FAIL=0

# ── Harness ───────────────────────────────────────────────────────────────────

# Sets up a sandbox: stub binaries, empty state, canned platform responses.
new_sandbox() {
    # Exported here rather than on run_setup's env prefix: the curl stub reads it,
    # and a prefix assignment is not visible to the other assignments beside it.
    SANDBOX=$(mktemp -d)
    export SANDBOX
    mkdir -p "$SANDBOX/bin" "$SANDBOX/state" "$SANDBOX/resp"

    # curl stub. Dispatches on the request to a canned response file:
    #   resp/verify   — GET  /tunnels/{id}
    #   resp/create   — POST /tunnels
    #   resp/records  — POST /tunnels/{id}/records
    # Each file is "<http_code>\n<body>". Every call is appended to calls.log so
    # a test can assert what was and was not requested.
    cat >"$SANDBOX/bin/curl" <<'STUB'
#!/bin/sh
url=""; method="GET"; code_only=0
while [ $# -gt 0 ]; do
    case "$1" in
        -X) method="$2"; shift 2 ;;
        -o) [ "$2" = "/dev/null" ] && code_only=1; shift 2 ;;
        -H|-d|-w|--max-time) shift 2 ;;
        http*) url="$1"; shift ;;
        *) shift ;;
    esac
done
case "$method:$url" in
    GET:*/tunnels/*)     key=verify ;;
    POST:*/tunnels)      key=create ;;
    POST:*/records)      key=records ;;
    *)                   key=unknown ;;
esac
echo "$method $key" >> "$SANDBOX/calls.log"
file="$SANDBOX/resp/$key"
[ -f "$file" ] || { echo "000"; exit 0; }
http=$(head -1 "$file")
body=$(tail -n +2 "$file")
if [ "$code_only" = "1" ]; then
    printf '%s' "$http"
else
    printf '%s\n%s' "$body" "$http"
fi
STUB

    # wg stub — deterministic keys so the written config can be asserted exactly.
    cat >"$SANDBOX/bin/wg" <<'STUB'
#!/bin/sh
case "$1" in
    genkey) echo "PRIVKEY-generated" ;;
    pubkey) cat >/dev/null; echo "PUBKEY-derived" ;;
esac
STUB

    chmod +x "$SANDBOX/bin/curl" "$SANDBOX/bin/wg"
    : >"$SANDBOX/calls.log"
}

respond() { # respond <key> <http_code> <body>
    printf '%s\n%s' "$2" "$3" >"$SANDBOX/resp/$1"
}

write_state() { cat >"$SANDBOX/state/wg-state.json"; }

# Runs setup.sh in the sandbox. Stdout+stderr land in $OUT, exit code in $RC.
run_setup() {
    OUT="$SANDBOX/output.txt"
    WG_DIR="$SANDBOX/wireguard" \
        YOLAB_DIR="$SANDBOX/yolab" \
        STATE_FILE="$SANDBOX/state/wg-state.json" \
        PATH="$SANDBOX/bin:$PATH" \
        PLATFORM_API_URL="https://api.example.test" \
        ACCOUNT_TOKEN="test-token" \
        SERVICE_NAME="${SERVICE_NAME_OVERRIDE-myapp}" \
        sh "$SETUP" >"$OUT" 2>&1
    RC=$?
}

state() { cat "$SANDBOX/state/wg-state.json" 2>/dev/null; }
# Reads one field, so assertions do not depend on jq's output formatting.
state_field() { jq -r ".$1 // empty" "$SANDBOX/state/wg-state.json" 2>/dev/null; }
wg_conf() { cat "$SANDBOX/wireguard/wg0.conf" 2>/dev/null; }
env_file() { cat "$SANDBOX/yolab/env" 2>/dev/null; }
called() { grep -q "$1" "$SANDBOX/calls.log"; }

ok() { PASS=$((PASS + 1)); }
bad() {
    FAIL=$((FAIL + 1))
    printf 'FAIL %s\n     %s\n' "$CASE" "$1"
}

assert_contains() { # assert_contains <haystack> <needle> <what>
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
    unset SERVICE_NAME_OVERRIDE
}

# Canned platform payloads.
TUNNEL_BODY='{"tunnel_id":77,"sub_ipv6":"2001:db8::99","wg_server_endpoint":"1.2.3.4:51820","wg_server_public_key":"SERVER-PUB"}'
RECORD_BODY='{"fqdn":"myapp.example.test"}'

CACHED_STATE='{"tunnel_id":42,"sub_ipv6":"2001:db8::42","wg_private_key":"PRIVKEY-cached",
 "wg_server_endpoint":"9.9.9.9:51820","wg_server_public_key":"CACHED-SERVER-PUB","fqdn":"old.example.test"}'

# ── Fresh registration ────────────────────────────────────────────────────────

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

case_start "the env file exports what the app containers source"
respond create 200 "$TUNNEL_BODY"
respond records 200 "$RECORD_BODY"
run_setup
assert_contains "$(env_file)" 'export YOLAB_FQDN=myapp.example.test' "env"
assert_contains "$(env_file)" 'export YOLAB_URL=https://myapp.example.test' "env"
assert_contains "$(env_file)" 'export YOLAB_IPV6=2001:db8::99' "env"
case_end

# The state file holds a WireGuard private key. Group/world-readable would expose
# it to anything else sharing the RWX volume.
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

# ── Reuse of a cached tunnel ──────────────────────────────────────────────────

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

# A restore brings back state naming an OLD tunnel while the live DNS record may
# point at a newer one. POST /records is an upsert by name, so re-asserting on
# every reuse is what makes a restored app reachable again without intervention.
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

# ── Re-registration ───────────────────────────────────────────────────────────

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

# ── Surviving a platform outage ───────────────────────────────────────────────
#
# The distinction this script exists to make: "the platform said this tunnel is
# gone" (re-register) versus "the platform did not answer" (keep running). Losing
# it means every app in the cluster drops its tunnel during an outage of a service
# it does not otherwise need to be online.

case_start "an unreachable platform does not cost the app its tunnel"
write_state <<EOF
$CACHED_STATE
EOF
# No verify response file at all — the stub reports 000, curl's "no answer".
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

# ── Hard failures ─────────────────────────────────────────────────────────────
#
# These must exit non-zero: the init container has to fail so the pod retries,
# rather than letting the app start with no tunnel and appear healthy.

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

case_start "a missing account token fails immediately"
OUT="$SANDBOX/output.txt"
WG_DIR="$SANDBOX/wireguard" YOLAB_DIR="$SANDBOX/yolab" PATH="$SANDBOX/bin:$PATH" \
    STATE_FILE="$SANDBOX/state/wg-state.json" PLATFORM_API_URL="https://api.example.test" \
    sh "$SETUP" >"$OUT" 2>&1
RC=$?
if [ "$RC" -ne 0 ]; then ok; else bad "expected a non-zero exit, got $RC"; fi
assert_not_called "POST create" "nothing should be requested without a token"
case_end

# ── Result ────────────────────────────────────────────────────────────────────

echo "wg-register: $PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
