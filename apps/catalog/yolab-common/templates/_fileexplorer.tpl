{{- define "yolab-common.fileExplorer.wanted" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (hasKey $cfg "file_explorer_enabled") (eq (get $cfg "file_explorer_enabled") false) -}}
{{- else -}}true{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.yolab" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if ne (include "yolab-common.fileExplorer.wanted" .) "true" -}}
{{- else if hasKey $cfg "file_explorer_yolab_enabled" -}}
{{- if (get $cfg "file_explorer_yolab_enabled") -}}true{{- end -}}
{{- else -}}
{{- include "yolab-common.yolab.enabled" . -}}
{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.tor" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (eq (include "yolab-common.fileExplorer.wanted" .) "true") (get $cfg "file_explorer_tor_enabled") -}}true{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.tailscale" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (eq (include "yolab-common.fileExplorer.wanted" .) "true") (get $cfg "file_explorer_tailscale_enabled") -}}true{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.enabled" -}}
{{- if or (eq (include "yolab-common.fileExplorer.yolab" .) "true") (eq (include "yolab-common.fileExplorer.tor" .) "true") (eq (include "yolab-common.fileExplorer.tailscale" .) "true") -}}true{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.subdomain" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- (get $cfg "file_explorer_subdomain") | default (printf "%s-files" (((.Values.yolab).serviceName) | default .Release.Name)) -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.tailscaleHostname" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- (get $cfg "file_explorer_tailscale_hostname") | default (include "yolab-common.fileExplorer.subdomain" .) -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.username" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- (get $cfg "file_explorer_username") | default "admin" -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.readOnly" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (hasKey $cfg "file_explorer_read_only") (eq (get $cfg "file_explorer_read_only") true) -}}true{{- else -}}false{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.protected" -}}
{{- $folders := list "caddy" "file-explorer" "yolab-state" "tor" "tailscale" "file-explorer-tor" "file-explorer-tailscale" -}}
{{- range ((((.Values.yolab).fileExplorer).protect) | default list) -}}
{{- $folders = append $folders . -}}
{{- end -}}
{{- toJson ($folders | uniq) -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.port" -}}18790{{- end -}}

{{- define "yolab-common.fileExplorer.tailscalePort" -}}18793{{- end -}}

{{- define "yolab-common.fileExplorer.torPort" -}}18794{{- end -}}


{{- define "yolab-common.fileExplorerSecret" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
{{- $cfg := (.Values.config) | default dict }}
{{- $password := (get $cfg "file_explorer_password") | default "" }}
apiVersion: v1
kind: Secret
metadata:
  name: {{ printf "%s-file-explorer" .Release.Name }}
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  username: {{ include "yolab-common.fileExplorer.username" . | quote }}
  {{- if $password }}
  password: {{ $password | quote }}
  {{- end }}
  {{- if eq (include "yolab-common.fileExplorer.tailscale" .) "true" }}
  tailscale-authkey: {{ required "file_explorer_tailscale_auth_key is required when the file explorer's Tailscale is on" (get $cfg "file_explorer_tailscale_auth_key") | quote }}
  {{- end }}
{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorerInit" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
{{- $public := eq (include "yolab-common.fileExplorer.yolab" .) "true" }}
- name: file-explorer-init
  image: {{ include "yolab-common.image.caddy" . }}
  imagePullPolicy: IfNotPresent
  env:
    - name: EXPLORER_USER
      valueFrom:
        secretKeyRef:
          name: {{ printf "%s-file-explorer" .Release.Name }}
          key: username
    - name: EXPLORER_PASSWORD
      valueFrom:
        secretKeyRef:
          name: {{ printf "%s-file-explorer" .Release.Name }}
          key: password
          optional: true
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      . /yolab/env
      {{- if $public }}
      if [ -z "${FILE_EXPLORER_FQDN:-}" ]; then
        echo "wg-register exported no FILE_EXPLORER_FQDN — the explorer has no address" >&2
        exit 1
      fi
      {{- end }}
      case "$EXPLORER_USER" in
        ""|*[!A-Za-z0-9._-]*)
          echo "file explorer username '$EXPLORER_USER' may only use letters, digits, '.', '_' and '-'" >&2
          exit 1 ;;
      esac
      PWFILE=/browse-state/password
      if [ -n "${EXPLORER_PASSWORD:-}" ]; then
        printf '%s' "$EXPLORER_PASSWORD" > "$PWFILE"
      elif [ ! -s "$PWFILE" ]; then
        tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 24 > "$PWFILE"
      fi
      HASH=$(caddy hash-password --plaintext "$(cat "$PWFILE")")
      printf "export FILE_EXPLORER_USER='%s'\n" "$EXPLORER_USER" >> /yolab/env
      printf "export FILE_EXPLORER_AUTH_HASH='%s'\n" "$HASH" >> /yolab/env
      {{- if $public }}
      echo "YOLAB_OUTPUT file_explorer_url https://$FILE_EXPLORER_FQDN/"
      {{- end }}
      echo "YOLAB_OUTPUT file_explorer_username $EXPLORER_USER"
      echo "YOLAB_OUTPUT file_explorer_password $(cat "$PWFILE")"
  volumeMounts:
    - name: yolab
      mountPath: /yolab
    - name: data
      mountPath: /browse-state
      subPath: {{ printf "%s/file-explorer" .Release.Name | quote }}
{{- if eq (include "yolab-common.fileExplorer.tor" .) "true" }}
- name: file-explorer-tor-state
  image: {{ include "yolab-common.image.tor" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 0
    runAsGroup: 0
  env:
    {{- include "yolab-common.podNamespaceEnv" . | nindent 4 }}
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      {{- include "yolab-common.claimState" . | nindent 6 }}
      claim_state /var/lib/tor
      mkdir -p /var/lib/tor/service
      chown -R 100:101 /var/lib/tor
      chmod 700 /var/lib/tor /var/lib/tor/service
  volumeMounts:
    - name: data
      mountPath: /var/lib/tor
      subPath: {{ printf "%s/file-explorer-tor" .Release.Name | quote }}
{{- end }}
{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorerConfigMap" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
{{- $writable := ne (include "yolab-common.fileExplorer.readOnly" .) "true" }}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ printf "%s-file-explorer" .Release.Name }}
  namespace: {{ .Release.Namespace }}
data:
  config.yaml: |
    http:
      listen: "127.0.0.1"
      port: {{ include "yolab-common.fileExplorer.port" . }}
      disableWebDAV: true
    server:
      disableUpdateCheck: true
      cacheDir: /var/lib/filebrowser/cache
      database:
        path: /var/lib/filebrowser/database.sqlite
      filesystem:
        createFilePermission: "666"
        createDirectoryPermission: "777"
      sources:
        - path: /srv/data
          name: {{ .Chart.Name | quote }}
          config:
            defaultEnabled: true
            private: true
            readOnly: {{ not $writable }}
            defaultPermissions:
              view: true
              download: true
              modify: {{ $writable }}
              create: {{ $writable }}
              delete: {{ $writable }}
            rules:
              - folderPath: "/"
                viewable: true
    auth:
      methods:
        password:
          enabled: false
        proxy:
          enabled: true
          header: X-Yolab-User
    frontend:
      name: {{ printf "%s files" .Chart.Name | quote }}
      disableDefaultLinks: true
  {{- if eq (include "yolab-common.fileExplorer.tor" .) "true" }}
  torrc: |
    DataDirectory /var/lib/tor
    SocksPort 0
    Log notice stdout
    HiddenServiceDir /var/lib/tor/service
    HiddenServicePort 80 127.0.0.1:{{ include "yolab-common.fileExplorer.torPort" . }}
  {{- end }}
  {{- if eq (include "yolab-common.fileExplorer.tailscale" .) "true" }}
  serve.json: |
    {
      "TCP": {"443": {"HTTPS": true}},
      "Web": {
        "${TS_CERT_DOMAIN}:443": {
          "Handlers": {"/": {"Proxy": "http://127.0.0.1:{{ include "yolab-common.fileExplorer.tailscalePort" . }}"}}
        }
      },
      "AllowFunnel": {"${TS_CERT_DOMAIN}:443": false}
    }
  {{- end }}
{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorerContainer" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
- name: file-explorer
  image: {{ include "yolab-common.image.fileExplorer" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 0
    runAsGroup: 0
  env:
    - name: FILEBROWSER_CONFIG
      value: /etc/filebrowser/config.yaml
  volumeMounts:
    - name: file-explorer-config
      mountPath: /etc/filebrowser
      readOnly: true
    - name: file-explorer-state
      mountPath: /var/lib/filebrowser
    - name: data
      mountPath: /srv/data
      subPath: {{ .Release.Name | quote }}
      readOnly: {{ eq (include "yolab-common.fileExplorer.readOnly" .) "true" }}
    {{- range (include "yolab-common.fileExplorer.protected" . | fromJsonArray) }}
    - name: data
      mountPath: {{ printf "/srv/data/%s" . | quote }}
      subPath: {{ printf "%s/%s" $.Release.Name . | quote }}
      readOnly: true
    {{- end }}
{{- if eq (include "yolab-common.fileExplorer.tor" .) "true" }}
- name: file-explorer-tor
  image: {{ include "yolab-common.image.tor" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 100
    runAsGroup: 101
  command:
    - /bin/sh
    - -c
    - |
      set -u
      tor -f /etc/yolab-tor/torrc &
      pid=$!
      while kill -0 "$pid" 2>/dev/null; do
        if [ -s /var/lib/tor/service/hostname ]; then
          echo "YOLAB_OUTPUT file_explorer_tor_url http://$(cat /var/lib/tor/service/hostname)/"
          sleep 600 & wait $!
        else
          sleep 2
        fi
      done
      wait "$pid"
  volumeMounts:
    - name: file-explorer-config
      mountPath: /etc/yolab-tor
      readOnly: true
    - name: data
      mountPath: /var/lib/tor
      subPath: {{ printf "%s/file-explorer-tor" .Release.Name | quote }}
{{- end }}
{{- if eq (include "yolab-common.fileExplorer.tailscale" .) "true" }}
- name: file-explorer-tailscale
  image: {{ include "yolab-common.image.tailscale" . }}
  imagePullPolicy: IfNotPresent
  env:
    - name: TS_AUTHKEY
      valueFrom:
        secretKeyRef:
          name: {{ printf "%s-file-explorer" .Release.Name }}
          key: tailscale-authkey
    - name: TS_AUTH_ONCE
      value: "true"
    - name: TS_HOSTNAME
      value: {{ include "yolab-common.fileExplorer.tailscaleHostname" . | quote }}
    - name: TS_STATE_DIR
      value: /var/lib/tailscale
    - name: TS_USERSPACE
      value: "true"
    - name: TS_SOCKET
      value: /tmp/tailscaled.sock
    - name: TS_SERVE_CONFIG
      value: /etc/yolab-tailscale/serve.json
    - name: TS_KUBE_SECRET
      value: ""
    - name: TS_TAILSCALED_EXTRA_ARGS
      value: "--port=0"
    {{- include "yolab-common.podNamespaceEnv" . | nindent 4 }}
  command:
    - /bin/sh
    - -c
    - |
      set -u
      {{- include "yolab-common.claimState" . | nindent 6 }}
      claim_state /var/lib/tailscale
      /usr/local/bin/containerboot &
      pid=$!
      while kill -0 "$pid" 2>/dev/null; do
        name=$(tailscale --socket=/tmp/tailscaled.sock status --json --peers=false 2>/dev/null \
          | sed -n 's/.*"DNSName": *"\([^"]*\)\.".*/\1/p' | head -n 1)
        if [ -n "$name" ]; then
          echo "YOLAB_OUTPUT file_explorer_tailscale_url https://$name/"
          sleep 600 & wait $!
        else
          sleep 5
        fi
      done
      wait "$pid"
  volumeMounts:
    - name: file-explorer-config
      mountPath: /etc/yolab-tailscale
      readOnly: true
    - name: data
      mountPath: /var/lib/tailscale
      subPath: {{ printf "%s/file-explorer-tailscale" .Release.Name | quote }}
{{- end }}
{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorerVolumes" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
- name: file-explorer-config
  configMap:
    name: {{ printf "%s-file-explorer" .Release.Name }}
- name: file-explorer-state
  emptyDir: {}
{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.caddySite" -}}
{{- if eq (include "yolab-common.fileExplorer.yolab" .) "true" }}
{$FILE_EXPLORER_FQDN} {
  basic_auth {
    {$FILE_EXPLORER_USER} {$FILE_EXPLORER_AUTH_HASH}
  }
  reverse_proxy localhost:{{ include "yolab-common.fileExplorer.port" . }} {
    header_up X-Yolab-User {http.auth.user.id}
  }
}
{{- end }}
{{- if eq (include "yolab-common.fileExplorer.tailscale" .) "true" }}
http://:{{ include "yolab-common.fileExplorer.tailscalePort" . }} {
  bind 127.0.0.1
  basic_auth {
    {$FILE_EXPLORER_USER} {$FILE_EXPLORER_AUTH_HASH}
  }
  reverse_proxy localhost:{{ include "yolab-common.fileExplorer.port" . }} {
    header_up X-Yolab-User {http.auth.user.id}
    header_up X-Forwarded-Proto https
  }
}
{{- end }}
{{- if eq (include "yolab-common.fileExplorer.tor" .) "true" }}
http://:{{ include "yolab-common.fileExplorer.torPort" . }} {
  bind 127.0.0.1
  basic_auth {
    {$FILE_EXPLORER_USER} {$FILE_EXPLORER_AUTH_HASH}
  }
  reverse_proxy localhost:{{ include "yolab-common.fileExplorer.port" . }} {
    header_up X-Yolab-User {http.auth.user.id}
  }
}
{{- end }}
{{- end -}}
