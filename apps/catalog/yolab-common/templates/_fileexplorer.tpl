{{- define "yolab-common.fileExplorer.enabled" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (hasKey $cfg "file_explorer_enabled") (eq (get $cfg "file_explorer_enabled") false) -}}
{{- else -}}true{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.subdomain" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- (get $cfg "file_explorer_subdomain") | default (printf "%s-files" (((.Values.yolab).serviceName) | default .Release.Name)) -}}
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
{{- $folders := list "caddy" "file-explorer" "yolab-state" -}}
{{- range ((((.Values.yolab).fileExplorer).protect) | default list) -}}
{{- $folders = append $folders . -}}
{{- end -}}
{{- toJson ($folders | uniq) -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.port" -}}18790{{- end -}}


{{- define "yolab-common.fileExplorerSecret" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
{{- $password := (get ((.Values.config) | default dict) "file_explorer_password") | default "" }}
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
{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorerInit" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
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
      if [ -z "${FILE_EXPLORER_FQDN:-}" ]; then
        echo "wg-register exported no FILE_EXPLORER_FQDN — the explorer has no address" >&2
        exit 1
      fi
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
      echo "YOLAB_OUTPUT file_explorer_url https://$FILE_EXPLORER_FQDN/"
      echo "YOLAB_OUTPUT file_explorer_username $EXPLORER_USER"
      echo "YOLAB_OUTPUT file_explorer_password $(cat "$PWFILE")"
  volumeMounts:
    - name: yolab
      mountPath: /yolab
    - name: data
      mountPath: /browse-state
      subPath: {{ printf "%s/file-explorer" .Release.Name | quote }}
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
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
{$FILE_EXPLORER_FQDN} {
  basic_auth {
    {$FILE_EXPLORER_USER} {$FILE_EXPLORER_AUTH_HASH}
  }
  reverse_proxy localhost:{{ include "yolab-common.fileExplorer.port" . }} {
    header_up X-Yolab-User {http.auth.user.id}
  }
}
{{- end -}}
{{- end -}}
