


{{- define "yolab-common.auth.enabled" -}}
{{- if (((.Values.config).auth_enabled)) -}}true{{- end -}}
{{- end -}}

{{- define "yolab-common.image.authelia" -}}
{{- (((.Values.yolab).images).authelia) | default "docker.io/authelia/authelia:4.39.1" -}}
{{- end -}}


{{- define "yolab-common.autheliaSecret" -}}
{{- if eq (include "yolab-common.auth.enabled" .) "true" -}}
apiVersion: v1
kind: Secret
metadata:
  name: {{ printf "%s-auth" .Release.Name }}
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  # One "user\tpassword" per line. A tab, not a colon: a password may contain
  # a colon and splitting on the first one would silently truncate it, which
  # shows up much later as "my password does not work".
  logins: |
{{- range (((.Values.config).auth_users)) | default list }}
    {{ .username }}{{ "\t" }}{{ .password }}
{{- end }}
{{- end -}}
{{- end -}}


{{- define "yolab-common.autheliaInit" -}}
{{- if eq (include "yolab-common.auth.enabled" .) "true" -}}
- name: authelia-config
  image: {{ include "yolab-common.image.authelia" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      . /yolab/env
      if [ -z "${YOLAB_FQDN:-}" ]; then
        echo "no YOLAB_FQDN — refusing to configure Authelia against a blank host" >&2
        exit 1
      fi

      # Generated once and kept on the PVC. Regenerating them on every upgrade
      # would log everyone out and make the stored database unreadable.
      mkdir -p /data/secrets
      for s in jwt session storage; do
        [ -f "/data/secrets/$s" ] || \
          tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 64 > "/data/secrets/$s"
      done
      JWT=$(cat /data/secrets/jwt)
      SESSION=$(cat /data/secrets/session)
      STORAGE=$(cat /data/secrets/storage)

      cat > /authelia-config/configuration.yml <<EOF
      theme: light
      server:
        # The trailing path is what serves the portal under /authelia on the
        # app's own domain, so no second subdomain or tunnel is needed.
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
          - domain: '${YOLAB_FQDN}'
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
        # Nothing here sends mail. Password reset is disabled above, so this
        # exists only because Authelia requires a notifier to be configured.
        filesystem:
          filename: /data/notification.txt
      identity_validation:
        reset_password:
          jwt_secret: '${JWT}'
      EOF

      # Tab-separated, written by the chart from a structured list — so there
      # is no user-typed format to get wrong, and a password containing a colon
      # or a space survives intact.
      echo "users:" > /authelia-config/users_database.yml
      any=0
      while IFS="$(printf '\t')" read -r u p || [ -n "${u:-}" ]; do
        [ -n "$u" ] && [ -n "$p" ] || continue
        # Authelia's own hasher: the file backend accepts only the exact
        # argon2id encoding it produces.
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
        # Failing here is deliberate. Starting with an empty user file would
        # leave Authelia up and every login rejected, which reads as "my
        # password is wrong" rather than "no users were configured".
        echo "auth was enabled but no logins were given" >&2
        exit 1
      fi
      echo "configured Authelia for ${YOLAB_FQDN}"
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
      subPath: {{ printf "%s/authelia" .Release.Name | quote }}
{{- end -}}
{{- end -}}

{{- define "yolab-common.autheliaContainer" -}}
{{- if eq (include "yolab-common.auth.enabled" .) "true" -}}
- name: authelia
  image: {{ include "yolab-common.image.authelia" . }}
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
      subPath: {{ printf "%s/authelia" .Release.Name | quote }}
{{- end -}}
{{- end -}}

{{- define "yolab-common.autheliaVolumes" -}}
{{- if eq (include "yolab-common.auth.enabled" .) "true" -}}
- name: authelia-config
  emptyDir: {}
- name: authelia-logins
  secret:
    secretName: {{ printf "%s-auth" .Release.Name }}
{{- end -}}
{{- end -}}
