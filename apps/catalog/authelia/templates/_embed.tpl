{{- define "authelia.image" -}}
{{- (((.Values.yolab).images).authelia) | default "docker.io/authelia/authelia:4.39.28@sha256:bd97cff4fcbf715b5ff1f9ae286afbe6033afce385302520b0368122d43a6f54" -}}
{{- end -}}

{{- define "authelia.cookieName" -}}yolab_auth{{- end -}}

{{- define "authelia.embed.mode" -}}
{{- if ((.Values.config).auth_enabled) -}}
{{- $mode := ((.Values.config).auth_mode) | default "own" -}}
{{- if not (has $mode (list "own" "existing")) -}}
{{- fail (printf "config.auth_mode is %q, not own or existing" $mode) -}}
{{- end -}}
{{- $mode -}}
{{- end -}}
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
          echo "    password: \"${h}\""
          echo "    groups: [admins]"
        } >> /authelia-config/users_database.yml
        any=1
      done < /auth/logins
      if [ "$any" = "0" ]; then
        echo "this service's own login was chosen but no user was given" >&2
        exit 1
      fi
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
