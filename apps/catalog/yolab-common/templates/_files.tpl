{{- define "yolab-common.files.enabled" -}}
{{- if ((.Values.yolab).files) -}}true{{- else -}}false{{- end -}}
{{- end -}}


{{- define "yolab-common.files.folder" -}}
{{- $given := ((.Values.yolab).filesFolder) | default ((.Values.config | default dict).files_folder) | default "" -}}
{{- if $given -}}
{{- include "yolab-common.tree.path" $given -}}
{{- else -}}
{{- printf "%s.files" (include "yolab-common.instance" .) -}}
{{- end -}}
{{- end -}}


{{- define "yolab-common.files.claim" -}}
{{- printf "content-%s" (include "yolab-common.files.folder" . | sha256sum | trunc 8) -}}
{{- end -}}

{{- define "yolab-common.files.mountPath" -}}/files{{- end -}}


{{- define "yolab-common.files.path" -}}
{{- $ctx := index . 0 -}}
{{- $sub := index . 1 | default "" | trimAll "/" -}}
{{- if or (hasPrefix "/" (index . 1 | default "")) (regexMatch "(^|/)\\.\\.(/|$)" $sub) -}}
{{- fail (printf "%q is not a folder inside this app's files" (index . 1)) -}}
{{- end -}}
{{- if $sub -}}
{{- printf "%s/%s" (include "yolab-common.files.mountPath" $ctx) $sub -}}
{{- else -}}
{{- include "yolab-common.files.mountPath" $ctx -}}
{{- end -}}
{{- end -}}


{{- define "yolab-common.files" -}}
{{- if and (eq (include "yolab-common.files.enabled" .) "true") (not .Member) }}
{{ include "yolab-common.tree.volume" (list . (include "yolab-common.files.claim" .) (include "yolab-common.files.folder" .)) }}
{{- end }}
{{- end -}}


{{- define "yolab-common.filesVolume" -}}
{{- if eq (include "yolab-common.files.enabled" .) "true" }}
- name: files
  persistentVolumeClaim:
    claimName: {{ include "yolab-common.files.claim" . }}
{{- end }}
{{- end -}}


{{- define "yolab-common.filesMount" -}}
{{- if eq (include "yolab-common.files.enabled" .) "true" }}
- name: files
  mountPath: {{ include "yolab-common.files.mountPath" . }}
{{- end }}
{{- end -}}


{{- define "yolab-common.filesDirs" -}}
{{- $ctx := index . 0 -}}
{{- $dirs := index . 1 -}}
{{- if eq (include "yolab-common.files.enabled" $ctx) "true" }}
- name: files-dirs
  image: {{ include "yolab-common.image.wgRegister" $ctx }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 0
    runAsGroup: 0
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      chown 1000:1000 {{ include "yolab-common.files.mountPath" $ctx | quote }}
      {{- range $dirs }}
      mkdir -p {{ include "yolab-common.files.path" (list $ctx .) | quote }}
      chown 1000:1000 {{ include "yolab-common.files.path" (list $ctx .) | quote }}
      {{- end }}
  volumeMounts:
    {{- include "yolab-common.filesMount" $ctx | nindent 4 }}
{{- end }}
{{- end -}}
