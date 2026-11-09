{{- define "authelia.image" -}}
{{- (((.Values.yolab).images).authelia) | default "docker.io/authelia/authelia:4.39.28@sha256:bd97cff4fcbf715b5ff1f9ae286afbe6033afce385302520b0368122d43a6f54" -}}
{{- end -}}

{{- define "authelia.image.redis" -}}
{{- (((.Values.yolab).images).authelia_sessions) | default "redis:7.4-alpine@sha256:858f009f9709ce576febc734aa78b8f6d624b82571f9ddb6bda4377c833b3499" -}}
{{- end -}}

{{- define "authelia.image.busybox" -}}
{{- (((.Values.yolab).images).authelia_busybox) | default "busybox:1.38@sha256:fd7dc98638c8e305f4dc34e979f1c0fdfdcaeb0fbf8fcff77ae834b6da3d7e6e" -}}
{{- end -}}

{{- define "authelia.cookieName" -}}yolab_auth{{- end -}}

{{- define "authelia.embed.mode" -}}
{{- if or ((.Values.config).auth_enabled) ((.Values.config).oidc_enabled) -}}
{{- $mode := ((.Values.config).auth_mode) | default "own" -}}
{{- if not (has $mode (list "own" "existing")) -}}
{{- fail (printf "config.auth_mode is %q, not own or existing" $mode) -}}
{{- end -}}
{{- $mode -}}
{{- end -}}
{{- end -}}

{{- define "authelia.embed.oidc" -}}
{{- if ((.Values.config).oidc_enabled) -}}
{{- if ne (include "yolab-common.yolab.enabled" .) "true" -}}
{{- fail "config.oidc_enabled needs the YoLab address: Authelia sends people back to it after they sign in" -}}
{{- end -}}
true
{{- end -}}
{{- end -}}

{{- define "authelia.oidc.spec" -}}
{{- $spec := required "yolab.auth.oidc is required: the paths Authelia may send people back to, and how the service proves who it is" (((.Values.yolab).auth).oidc) -}}
{{- $redirects := required "yolab.auth.oidc.redirects is required: the service's callback paths" $spec.redirects -}}
{{- range $redirects -}}
{{- if not (regexMatch "^/[A-Za-z0-9._~/-]*$" .) -}}
{{- fail (printf "yolab.auth.oidc.redirects: %q is not a path on the service's own address" .) -}}
{{- end -}}
{{- end -}}
{{- $method := $spec.method | default "client_secret_basic" -}}
{{- if not (has $method (list "client_secret_basic" "client_secret_post")) -}}
{{- fail (printf "yolab.auth.oidc.method is %q, not client_secret_basic or client_secret_post" $method) -}}
{{- end -}}
{{- $scopes := $spec.scopes | default (list "openid" "profile" "email" "groups") -}}
{{- if not (has "openid" $scopes) -}}
{{- fail "yolab.auth.oidc.scopes must include openid" -}}
{{- end -}}
{{- range $scopes -}}
{{- if not (has . (list "openid" "profile" "email" "groups" "offline_access")) -}}
{{- fail (printf "yolab.auth.oidc.scopes: %q is not one of openid, profile, email, groups, offline_access" .) -}}
{{- end -}}
{{- end -}}
{{- $name := $spec.name | default .Chart.Name -}}
{{- if not $spec.name -}}
{{- with .Chart.Annotations -}}
{{- with index . "yolab.io/display-name" -}}
{{- $name = . -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- if not (regexMatch "^[A-Za-z0-9 ._-]{1,64}$" $name) -}}
{{- fail (printf "yolab.auth.oidc.name %q may only hold letters, digits, spaces, dots, dashes and underscores" $name) -}}
{{- end -}}
{{- $grants := ternary (list "authorization_code" "refresh_token") (list "authorization_code") (has "offline_access" $scopes) -}}
{{- toJson (dict "redirects" $redirects "method" $method "scopes" $scopes "name" $name "grants" $grants) -}}
{{- end -}}

{{- define "authelia.oidc.provider" -}}
oidc_provider() {
  echo "identity_providers:"
  echo "  oidc:"
  echo "    hmac_secret: '$(cat "$1/oidc-hmac")'"
  echo "    jwks:"
  echo "      - key_id: yolab"
  echo "        algorithm: RS256"
  echo "        use: sig"
  echo "        key: |"
  sed 's/^/          /' "$1/oidc/private.pem"
  echo "    claims_policies:"
  echo "      yolab:"
  echo "        id_token: [email, email_verified, preferred_username, name, groups]"
  echo "    clients:"
}
{{- end -}}

{{- define "authelia.oidc.envFile" -}}
{
  printf 'export OIDC_ISSUER=%s\n' "$ISSUER"
  printf 'export OIDC_DISCOVERY_URL=%s/.well-known/openid-configuration\n' "$ISSUER"
  printf 'export OIDC_CLIENT_ID=%s\n' "$YOLAB_FQDN"
  printf 'export OIDC_CLIENT_SECRET=%s\n' "$(cat "$CLIENT_SECRET")"
  printf 'export OIDC_PROVIDER_NAME=Authelia\n'
} > /yolab/oidc.env.new
mv /yolab/oidc.env.new /yolab/oidc.env
echo "signing people in over OpenID with ${ISSUER} as ${YOLAB_FQDN}"
{{- end -}}

{{- define "authelia.oidc.clientSecret" -}}
if [ ! -s "$CLIENT_SECRET" ]; then
  tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 64 > "$CLIENT_SECRET.new"
  mv "$CLIENT_SECRET.new" "$CLIENT_SECRET"
fi
{{- end -}}

{{- define "authelia.oidc.pinHost" -}}
pin_host() {
  host=$1
  shift
  grep -v "[[:space:]]$host\$" /etc/hosts > /tmp/hosts.new || [ $? -eq 1 ]
  for ip in "$@"; do
    printf "%s %s\n" "$ip" "$host" >> /tmp/hosts.new
  done
  cat /tmp/hosts.new > /etc/hosts
}
{{- end -}}

{{- define "authelia.embed.upstream" -}}
{{- $raw := required "config.auth_url is required: the address of the Authelia this service signs people in with" ((.Values.config).auth_url) | trim -}}
{{- $url := urlParse $raw -}}
{{- if not (has $url.scheme (list "http" "https")) -}}
{{- fail (printf "config.auth_url %q must start with http:// or https://" $raw) -}}
{{- end -}}
{{- if not $url.host -}}
{{- fail (printf "config.auth_url %q has no host" $raw) -}}
{{- end -}}
{{- if or $url.query $url.fragment -}}
{{- fail (printf "config.auth_url %q must not carry a query or a fragment" $raw) -}}
{{- end -}}
{{- toJson (dict "upstream" (printf "%s://%s" $url.scheme $url.host) "path" (trimSuffix "/" $url.path) "tls" (eq $url.scheme "https")) -}}
{{- end -}}

{{- define "authelia.embed.secret" -}}
{{- if eq (include "authelia.embed.mode" .) "own" -}}
{{- if not ((.Values.config).auth_users) -}}
{{- fail "config.auth_users is required: this service's own login needs at least one user" -}}
{{- end -}}
apiVersion: v1
kind: Secret
metadata:
  name: {{ printf "%s-auth" .Release.Name }}
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  logins: |
{{- range ((.Values.config).auth_users) }}
    {{ .username }}{{ "\t" }}{{ .password }}
{{- end }}
{{- end -}}
{{- end -}}

{{- define "authelia.embed.init" -}}
{{- $oidc := eq (include "authelia.embed.oidc" .) "true" -}}
{{- $spec := dict -}}
{{- if $oidc }}{{ $spec = include "authelia.oidc.spec" . | fromJson }}{{ end -}}
{{- if eq (include "authelia.embed.mode" .) "own" -}}
- name: authelia-config
  image: {{ include "authelia.image" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      . /yolab/env
      if [ -z "${YOLAB_FQDN:-}" ]; then
        echo "no YOLAB_FQDN: refusing to configure Authelia against a blank host" >&2
        exit 1
      fi
      mkdir -p /data/secrets
      for s in jwt session storage; do
        [ -f "/data/secrets/$s" ] || \
          tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 64 > "/data/secrets/$s"
      done
      {{- if $oidc }}
      for s in oidc-hmac oidc-client; do
        if [ ! -s "/data/secrets/$s" ]; then
          tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 64 > "/data/secrets/$s.new"
          mv "/data/secrets/$s.new" "/data/secrets/$s"
        fi
      done
      if [ ! -s /data/secrets/oidc/private.pem ]; then
        rm -rf /data/secrets/oidc.new
        mkdir -p /data/secrets/oidc.new
        authelia crypto pair rsa generate --bits 2048 --directory /data/secrets/oidc.new
        rm -rf /data/secrets/oidc
        mv /data/secrets/oidc.new /data/secrets/oidc
      fi
      {{- end }}
      JWT=$(cat /data/secrets/jwt)
      SESSION=$(cat /data/secrets/session)
      STORAGE=$(cat /data/secrets/storage)
      COOKIE_DOMAIN="$YOLAB_FQDN"
      {{- if (((.Values.yolab).auth).sharedSession) }}
      COOKIE_DOMAIN="${YOLAB_FQDN#*.}"
      {{- end }}
      cat > /authelia-config/configuration.yml <<EOF
      theme: auto
      server:
        address: 'tcp://:9091/authelia'
      log:
        level: info
      authentication_backend:
        password_reset:
          disable: true
        file:
          path: /authelia-config/users_database.yml
      access_control:
        default_policy: one_factor
      session:
        name: authelia_session
        secret: '${SESSION}'
        cookies:
          - domain: '${COOKIE_DOMAIN}'
            authelia_url: 'https://${YOLAB_FQDN}/authelia'
      regulation:
        max_retries: 5
        find_time: 2m
        ban_time: 5m
      storage:
        encryption_key: '${STORAGE}'
        local:
          path: /data/db.sqlite3
      notifier:
        filesystem:
          filename: /data/notification.txt
      identity_validation:
        reset_password:
          jwt_secret: '${JWT}'
      EOF
      echo "users:" > /authelia-config/users_database.yml
      any=0
      while IFS="$(printf '\t')" read -r u p || [ -n "${u:-}" ]; do
        [ -n "$u" ] && [ -n "$p" ] || continue
        h=$(authelia crypto hash generate argon2 --password "$p" | sed 's/^Digest: //')
        {
          echo "  ${u}:"
          echo "    disabled: false"
          echo "    displayname: \"${u}\""
          echo "    email: \"${u}@yolab.local\""
          echo "    password: \"${h}\""
          echo "    groups: [admins]"
        } >> /authelia-config/users_database.yml
        any=1
      done < /auth/logins
      if [ "$any" = "0" ]; then
        echo "this service's own login was chosen but no user was given" >&2
        exit 1
      fi
      {{- if $oidc }}
      {{- include "authelia.oidc.provider" . | nindent 6 }}
      {
        oidc_provider /data/secrets
        echo "      - client_id: '${YOLAB_FQDN}'"
        echo "        client_name: '{{ $spec.name }}'"
        echo "        client_secret: '\$plaintext\$$(cat /data/secrets/oidc-client)'"
        echo "        public: false"
        echo "        authorization_policy: one_factor"
        echo "        consent_mode: implicit"
        echo "        claims_policy: yolab"
        echo "        redirect_uris:"
        {{- range $spec.redirects }}
        echo "          - 'https://${YOLAB_FQDN}{{ . }}'"
        {{- end }}
        echo "        scopes: [{{ join ", " $spec.scopes }}]"
        echo "        grant_types: [{{ join ", " $spec.grants }}]"
        echo "        response_types: [code]"
        echo "        token_endpoint_auth_method: '{{ $spec.method }}'"
      } >> /authelia-config/configuration.yml
      {{- end }}
      echo "configured this service's own Authelia for ${YOLAB_FQDN}"
  volumeMounts:
    - name: yolab
      mountPath: /yolab
    - name: authelia-config
      mountPath: /authelia-config
    - name: authelia-logins
      mountPath: /auth
      readOnly: true
    - name: data
      mountPath: /data
      subPath: "authelia"
{{- end -}}
{{- end -}}

{{- define "authelia.oidc.clientInit" -}}
{{- if eq (include "authelia.embed.oidc" .) "true" }}
{{- $mode := include "authelia.embed.mode" . }}
{{- $spec := include "authelia.oidc.spec" . | fromJson }}
- name: oidc-client
  image: {{ include "authelia.image.busybox" . }}
  imagePullPolicy: IfNotPresent
  {{- if eq $mode "existing" }}
  {{- $to := include "authelia.embed.upstream" . | fromJson }}
  {{- $key := (urlParse (((.Values.config).auth_url) | trim)).userinfo }}
  {{- if not $key }}
  {{- fail "config.auth_url carries no key: paste the Authelia service's latest \"Address for your other services\" output, which lets this service sign up for OpenID" }}
  {{- end }}
  env:
    - name: REGISTRAR_KEY
      value: {{ $key | quote }}
    - name: REGISTRAR_HOST
      value: {{ (urlParse $to.upstream).hostname | quote }}
  {{- end }}
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      . /yolab/env
      if [ -z "${YOLAB_FQDN:-}" ]; then
        echo "no YOLAB_FQDN: OpenID needs the YoLab address to send people back to" >&2
        exit 1
      fi
      {{- include "authelia.oidc.pinHost" . | nindent 6 }}
      address_of() {
        nslookup "$1" | awk '/^Name:/ { named = 1 } named && /^Address/ { print $NF; exit }'
      }
      CLIENT_SECRET=/state/secrets/oidc-client
      {{- if eq $mode "own" }}
      ISSUER="https://${YOLAB_FQDN}/authelia"
      waited=0
      until [ -s "$CLIENT_SECRET" ]; do
        if [ "$waited" -ge 300 ]; then
          echo "this service's own Authelia has not made its OpenID secret in 300s" >&2
          exit 1
        fi
        sleep 2
        waited=$((waited + 2))
      done
      PORTAL=authelia-portal.{{ .Release.Namespace }}.svc.cluster.local
      ADDRESS=$(address_of "$PORTAL")
      if [ -z "$ADDRESS" ]; then
        echo "cannot resolve $PORTAL" >&2
        exit 1
      fi
      pin_host "$YOLAB_FQDN" "$ADDRESS"
      {{- else }}
      mkdir -p /state/secrets
      {{- include "authelia.oidc.clientSecret" . | nindent 6 }}
      REDIRECTS=""
      {{- range $spec.redirects }}
      REDIRECTS="${REDIRECTS:+$REDIRECTS,}https://${YOLAB_FQDN}{{ . }}"
      {{- end }}
      BODY="key=${REGISTRAR_KEY}&client=${YOLAB_FQDN}&secret=$(cat "$CLIENT_SECRET")&name={{ $spec.name | replace " " "+" }}&method={{ $spec.method }}&scopes={{ join "," $spec.scopes }}&redirects=${REDIRECTS}"
      attempt=0
      until ANSWER=$(wget -q -T 150 -O - --post-data "$BODY" "http://${REGISTRAR_HOST}:9092/cgi-bin/register"); do
        attempt=$((attempt + 1))
        if [ "$attempt" -ge 10 ]; then
          echo "the Authelia at ${REGISTRAR_HOST} refused or did not answer the OpenID sign-up 10 times; its registrar log says why" >&2
          exit 1
        fi
        echo "OpenID sign-up with ${REGISTRAR_HOST} failed, trying again in 10s"
        sleep 10
      done
      ISSUER=$(printf '%s\n' "$ANSWER" | sed -n 's/^issuer //p' | head -n 1)
      case "$ISSUER" in
        https://*) ;;
        *)
          echo "the Authelia at ${REGISTRAR_HOST} answered without an issuer: ${ANSWER}" >&2
          exit 1
          ;;
      esac
      ISSUER_HOST=${ISSUER#https://}
      ISSUER_HOST=${ISSUER_HOST%%/*}
      ADDRESS=$(address_of "$REGISTRAR_HOST")
      if [ -z "$ADDRESS" ]; then
        echo "cannot resolve ${REGISTRAR_HOST}" >&2
        exit 1
      fi
      pin_host "$ISSUER_HOST" "$ADDRESS"
      {{- end }}
      {{- include "authelia.oidc.envFile" . | nindent 6 }}
  volumeMounts:
    - name: yolab
      mountPath: /yolab
    - name: data
      mountPath: /state
      subPath: "authelia"
{{- end }}
{{- end -}}

{{- define "authelia.embed.service" -}}
{{- if eq (include "yolab-common.oidc.ownPortal" .) "true" -}}
apiVersion: v1
kind: Service
metadata:
  name: authelia-portal
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: gateway
  ports:
    - name: https
      port: 443
      targetPort: 443
{{- end -}}
{{- end -}}

{{- define "authelia.embed.container" -}}
{{- $mode := include "authelia.embed.mode" . -}}
{{- if eq $mode "own" -}}
- name: authelia
  image: {{ include "authelia.image" . }}
  imagePullPolicy: IfNotPresent
  args: ["--config", "/authelia-config/configuration.yml"]
  ports:
    - containerPort: 9091
  readinessProbe:
    httpGet:
      path: /authelia/api/health
      port: 9091
    initialDelaySeconds: 10
    periodSeconds: 10
  volumeMounts:
    - name: authelia-config
      mountPath: /authelia-config
    - name: data
      mountPath: /data
      subPath: "authelia"
{{- else if eq $mode "existing" -}}
{{- $to := include "authelia.embed.upstream" . | fromJson -}}
- name: authelia
  image: {{ include "yolab-common.image.caddy" . }}
  imagePullPolicy: IfNotPresent
  env:
    - name: AUTH_UPSTREAM
      value: {{ $to.upstream | quote }}
    - name: AUTH_PATH
      value: {{ $to.path | quote }}
    - name: AUTH_COOKIE
      value: {{ include "authelia.cookieName" . | quote }}
    - name: XDG_CONFIG_HOME
      value: /tmp/caddy-config
    - name: XDG_DATA_HOME
      value: /tmp/caddy-data
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      . /yolab/env
      COOKIE_DOMAIN="${YOLAB_FQDN#*.}"
      cat > /tmp/Caddyfile <<EOF
      {
        admin off
        auto_https off
        servers {
          trusted_proxies static 127.0.0.1/32 ::1/128
        }
      }

      http://:9091 {
        bind 127.0.0.1 ::1
        handle /authelia/logout {
          header Set-Cookie "${AUTH_COOKIE}=; Domain=${COOKIE_DOMAIN}; Path=/; Max-Age=0; Secure; HttpOnly; SameSite=Lax"
          redir / 302
        }
        handle_path /authelia/* {
          rewrite * ${AUTH_PATH}{uri}
          reverse_proxy ${AUTH_UPSTREAM} {
            {{- if $to.tls }}
            header_up Host {upstream_hostport}
            {{- end }}
          }
        }
      }
      EOF
      echo "signing people in with the Authelia at ${AUTH_UPSTREAM}${AUTH_PATH}"
      exec caddy run --config /tmp/Caddyfile --adapter caddyfile
  readinessProbe:
    exec:
      command:
        - wget
        - -q
        - -O
        - /dev/null
        - http://127.0.0.1:9091/authelia/api/health
    initialDelaySeconds: 5
    periodSeconds: 10
  volumeMounts:
    - name: yolab
      mountPath: /yolab
{{- end -}}
{{- end -}}

{{- define "authelia.embed.volumes" -}}
{{- if eq (include "authelia.embed.mode" .) "own" -}}
- name: authelia-config
  emptyDir: {}
- name: authelia-logins
  secret:
    secretName: {{ printf "%s-auth" .Release.Name }}
{{- end -}}
{{- end -}}

{{- define "authelia.image.lldap" -}}
{{- (((.Values.yolab).images).lldap) | default "docker.io/lldap/lldap:2026-09-22-alpine@sha256:bb6e509b4a44e8e9985acfd95d83a85e252ed81879254d9751b9926e3ac26c62" -}}
{{- end -}}
