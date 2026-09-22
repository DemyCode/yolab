


{{- define "yolab-common.fileExplorer.enabled" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (hasKey $cfg "file_explorer_enabled") (eq (get $cfg "file_explorer_enabled") false) -}}
{{- else -}}true{{- end -}}
{{- end -}}


{{- define "yolab-common.fileExplorerInit" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
- name: file-explorer-init
  image: {{ include "yolab-common.image.caddy" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      . /yolab/env
      PWFILE=/browse-state/password
      [ -f "$PWFILE" ] || tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 24 > "$PWFILE"
      HASH=$(caddy hash-password --plaintext "$(cat "$PWFILE")")
      
      printf "export FILE_EXPLORER_AUTH_HASH='%s'\n" "$HASH" >> /yolab/env
      echo "YOLAB_OUTPUT file_explorer_url https://${YOLAB_FQDN}/__yolab-files/"
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


{{- define "yolab-common.fileExplorer.caddyHandle" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
handle /__yolab-files/* {
  basic_auth {
    explorer {$FILE_EXPLORER_AUTH_HASH}
  }
  uri strip_prefix /__yolab-files
  root * /browse
  file_server browse
}
{{- end -}}
{{- end -}}
