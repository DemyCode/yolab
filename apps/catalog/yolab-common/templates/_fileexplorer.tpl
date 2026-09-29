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


{{- define "yolab-common.fileExplorer.volumeMounts" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
- name: data
  mountPath: /browse
  subPath: {{ .Release.Name | quote }}
  readOnly: true
{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorer.caddySite" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
{$FILE_EXPLORER_FQDN} {
  basic_auth {
    {$FILE_EXPLORER_USER} {$FILE_EXPLORER_AUTH_HASH}
  }
  root * /browse
  file_server browse
}
{{- end -}}
{{- end -}}
