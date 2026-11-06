{{- define "yolab-common.files.enabled" -}}
{{- if ((.Values.yolab).files) -}}true{{- else -}}false{{- end -}}
{{- end -}}


{{- define "yolab-common.files.claim" -}}content{{- end -}}

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
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {{ include "yolab-common.files.claim" . }}
  namespace: {{ .Release.Namespace }}
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: yolab-cephfs
  resources:
    requests:
      storage: {{ ((.Values.yolab).filesSize) | default "1Ti" | quote }}
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
