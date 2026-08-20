{{/*
Authelia, bundled into an app's own gateway pod.

WHY IT LIVES IN THE APP AND NOT ON THE PLATFORM
-----------------------------------------------
Everything in this system is an app: installed, updated, backed up and removed
the same way. A shared auth service would be the one component with its own
lifecycle rules, and that special-casing spreads. So an app that wants a login
carries its own Authelia.

The cost is real and worth stating: no SSO, and a separate user list per app.
The benefit is that it is self-contained — uninstalling the app takes its auth
with it, and nothing else can be locked out by it.

WHY IT IS SIMPLER THAN A SHARED ONE
-----------------------------------
The portal lives on the app's OWN domain under /authelia, so the session cookie
is scoped to that single host. No parent-domain cookie, no second tunnel, and
no cross-domain redirect — which is the part that produces redirect loops that
only show up in a real browser.

THE ORDERING PROBLEM
--------------------
Authelia needs the app's public FQDN in its config (cookie domain, portal URL,
access-control rule), but that is not known when Helm renders: wg-register
claims the subdomain at runtime and writes it to /yolab/env. So the config is
rendered by an init container that runs AFTER wg-register — init containers run
in declaration order — and lands in an emptyDir the daemon then reads.

That same init container hashes the passwords. Authelia's file backend wants
argon2id, and the only tool that produces the exact format it accepts is
Authelia itself, so the hashing runs in the Authelia image rather than being
reimplemented.
*/}}

{{/* True when the app has been asked for a login. */}}
{{- define "yolab-common.auth.enabled" -}}
{{- if (((.Values.config).auth_enabled)) -}}true{{- end -}}
{{- end -}}

{{- define "yolab-common.image.authelia" -}}
{{- (((.Values.yolab).images).authelia) | default "docker.io/authelia/authelia:4.39.1" -}}
{{- end -}}

{{/*
Secret holding the plaintext logins exactly as typed on the install form.

Plaintext, deliberately: Authelia's file backend needs an argon2id hash it
produced itself, and hashing has to happen somewhere. Doing it here would mean
reimplementing Authelia's exact format; doing it in the init container means
the plaintext exists for the seconds between Secret and hash. It never reaches
the users file, which holds only hashes.
*/}}
{{- define "yolab-common.autheliaSecret" -}}
{{- if eq (include "yolab-common.auth.enabled" .) "true" -}}
apiVersion: v1
kind: Secret
metadata:
  name: {{ printf "%s-auth" .Release.Name }}
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  logins: |
{{ (((.Values.config).auth_users)) | default "" | indent 4 }}
{{- end -}}
{{- end -}}

{{/*
Init container: render the config, mint the secrets, hash the logins.

Runs after wg-register (declaration order) so /yolab/env carries YOLAB_FQDN.
Everything it writes to /authelia-config is an emptyDir; the long-lived state
(sqlite db, generated secrets) goes on the app's PVC so a restart does not log
everyone out or invalidate the database.
*/}}
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

      # One "user:password" per line, blank lines and # comments ignored.
      echo "users:" > /authelia-config/users_database.yml
      any=0
      while IFS= read -r line || [ -n "$line" ]; do
        case "$line" in ''|'#'*) continue ;; esac
        u=${line%%:*}
        p=${line#*:}
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
        echo "auth was enabled but no usable 'user:password' lines were given" >&2
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
