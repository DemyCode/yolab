#!/bin/sh
set -eu
STATE=${REGISTRAR_STATE:-/state}
SETTINGS=${REGISTRAR_SETTINGS:-/run/registrar/env}
WAIT=${REGISTRAR_WAIT:-120}
. "$SETTINGS"

reply() {
  printf 'HTTP/1.0 %s\r\nContent-Type: text/plain\r\nCache-Control: no-store\r\n\r\n%s\n' "$1" "$2"
  exit 0
}

[ "${REQUEST_METHOD:-}" = POST ] || reply "405 Method Not Allowed" "send the registration as a POST"
LEN=${CONTENT_LENGTH:-}
case "$LEN" in
  '' | *[!0-9]*) reply "411 Length Required" "the registration has no length" ;;
esac
[ "$LEN" -le 4096 ] || reply "413 Payload Too Large" "a registration is at most 4096 bytes"
BODY=$(head -c "$LEN")

field() {
  printf '%s\n' "$BODY" | tr '&' '\n' | sed -n "s/^$1=//p" | head -n 1
}

KEY=$(field key)
WANT=$(cat "$STATE/secrets/registrar-key")
[ -n "$KEY" ] && [ "$KEY" = "$WANT" ] || reply "403 Forbidden" "wrong key: paste this Authelia's latest \"Address for your other services\""

CLIENT=$(field client)
printf '%s' "$CLIENT" | grep -Eq '^([a-z0-9]([a-z0-9-]*[a-z0-9])?\.)+[a-z0-9]([a-z0-9-]*[a-z0-9])?$' \
  || reply "400 Bad Request" "client must be the service's own host name"
case "$CLIENT" in
  *."$PARENT") ;;
  *) reply "403 Forbidden" "only services under $PARENT can sign in with this Authelia" ;;
esac

SECRET=$(field secret)
printf '%s' "$SECRET" | grep -Eq '^[A-Za-z0-9]{32,128}$' \
  || reply "400 Bad Request" "secret must be 32 to 128 letters and digits"

NAME=$(field name | tr '+' ' ')
printf '%s' "$NAME" | grep -Eq '^[A-Za-z0-9 ._-]{1,64}$' \
  || reply "400 Bad Request" "name must be 1 to 64 letters, digits, spaces, dots, dashes or underscores"

METHOD=$(field method)
case "$METHOD" in
  client_secret_basic | client_secret_post) ;;
  *) reply "400 Bad Request" "method must be client_secret_basic or client_secret_post" ;;
esac

SCOPES=$(field scopes)
printf '%s' "$SCOPES" | grep -Eq '^[a-z_,]*$' || reply "400 Bad Request" "scopes are lowercase names separated by commas"
SCOPES=$(printf '%s' "$SCOPES" | tr ',' ' ')
GRANTS="authorization_code"
HAS_OPENID=0
for s in $SCOPES; do
  case "$s" in
    openid) HAS_OPENID=1 ;;
    profile | email | groups) ;;
    offline_access) GRANTS="authorization_code, refresh_token" ;;
    *) reply "400 Bad Request" "scope $s is not one of openid, profile, email, groups, offline_access" ;;
  esac
done
[ "$HAS_OPENID" = 1 ] || reply "400 Bad Request" "scopes must include openid"

REDIRECTS=$(field redirects)
printf '%s' "$REDIRECTS" | grep -Eq '^[A-Za-z0-9._~/:,-]*$' || reply "403 Forbidden" "a redirect may only hold letters, digits and ._~/:-"
REDIRECTS=$(printf '%s' "$REDIRECTS" | tr ',' ' ')
[ -n "$REDIRECTS" ] || reply "400 Bad Request" "at least one redirect is needed"
for r in $REDIRECTS; do
  printf '%s' "$r" | grep -Eq "^https://$(printf '%s' "$CLIENT" | sed 's/\./\\./g')/[A-Za-z0-9._~/-]*$" \
    || reply "403 Forbidden" "redirect $r is not a path on https://$CLIENT"
done

mkdir -p "$STATE/clients"
FILE="$STATE/clients/$CLIENT.yml"
TMP=$(mktemp "$STATE/clients/.$CLIENT.XXXXXX")
{
  echo "      - client_id: '$CLIENT'"
  echo "        client_name: '$NAME'"
  echo "        client_secret: '\$plaintext\$$SECRET'"
  echo "        public: false"
  echo "        authorization_policy: '$POLICY'"
  echo "        consent_mode: implicit"
  echo "        claims_policy: yolab"
  echo "        redirect_uris:"
  for r in $REDIRECTS; do
    echo "          - '$r'"
  done
  echo "        scopes: [$(echo $SCOPES | sed 's/ /, /g')]"
  echo "        grant_types: [$GRANTS]"
  echo "        response_types: [code]"
  echo "        token_endpoint_auth_method: '$METHOD'"
} > "$TMP"
if [ -f "$FILE" ] && cmp -s "$TMP" "$FILE"; then
  rm "$TMP"
else
  mv "$TMP" "$FILE"
fi

clients() {
  cat "$STATE"/clients/*.yml | md5sum | cut -d' ' -f1
}
waited=0
until [ -f "$STATE/clients.live" ] && [ "$(cat "$STATE/clients.live")" = "$(clients)" ]; do
  [ "$waited" -lt "$WAIT" ] || reply "504 Gateway Timeout" "Authelia did not come back with $CLIENT in ${WAIT}s"
  sleep 1
  waited=$((waited + 1))
done
reply "200 OK" "issuer $ISSUER"
