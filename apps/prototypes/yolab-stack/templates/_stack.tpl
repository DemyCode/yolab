{{- define "yolab-stack.prefix" -}}
{{- with .Member }}{{ . }}-{{ end -}}
{{- end -}}


{{- define "yolab-stack.outputKey" -}}
{{- $ctx := index . 0 -}}
{{- with $ctx.Member }}{{ . }}_{{ end }}{{ index . 1 }}
{{- end -}}


{{- define "yolab-stack.memberRelease" -}}
{{- $root := index . 0 -}}
{{- printf "%s-%s" $root.Release.Name (index . 1) -}}
{{- end -}}


{{- define "yolab-stack.member" -}}
{{- $root := index . 0 -}}
{{- $name := index . 1 -}}
{{- $config := index . 2 -}}
{{- $values := deepCopy (index $root.Values $name | default dict) -}}
{{- $cfg := deepCopy ($values.config | default dict) -}}
{{- range $key, $value := $config }}{{ if not (kindIs "invalid" $value) }}{{ $_ := set $cfg $key $value }}{{ end }}{{ end -}}
{{- $yolab := deepCopy ($values.yolab | default dict) -}}
{{- range $key, $value := ($root.Values.yolab | default dict) }}{{ $_ := set $yolab $key $value }}{{ end -}}
{{- $_ := set $yolab "serviceName" "" -}}
{{- with (($root.Values.yolab).serviceName) }}{{ $_ := set $yolab "serviceName" (printf "%s-%s" . $name) }}{{ end -}}
{{- $_ := set $values "config" $cfg -}}
{{- $_ := set $values "yolab" $yolab -}}
{{- $release := deepCopy $root.Release -}}
{{- $_ := set $release "Name" (include "yolab-stack.memberRelease" (list $root $name)) -}}
{{- $ctx := dict
      "Values" $values
      "Release" $release
      "Chart" (dict "Name" $name)
      "Capabilities" $root.Capabilities
      "Template" $root.Template
      "Files" $root.Files
      "Member" $name -}}
{{ include (printf "%s.manifests" $name) $ctx }}
{{- end -}}


{{- define "yolab-stack.folderClaim" -}}
{{- $root := index . 0 -}}
{{- $folder := index . 1 -}}
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: folder-{{ $folder }}
  namespace: {{ $root.Release.Namespace }}
  labels:
    yolab.io/folder: {{ $folder | quote }}
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: ""
  volumeName: {{ $root.Release.Namespace }}.folder-{{ $folder }}
  resources:
    requests:
      storage: 1Gi
{{- end -}}


{{- define "yolab-stack.ownFolderClaim" -}}
{{- if and (not .Member) .Values.config.media_folder }}
---
{{ include "yolab-stack.folderClaim" (list . .Values.config.media_folder) }}
{{- end }}
{{- end -}}


{{- define "yolab-stack.urlOutput" -}}
{{- if and .Member (eq (include "yolab-common.yolab.enabled" .) "true") }}
- name: stack-output
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      . /yolab/env
      if [ -n "${YOLAB_FQDN:-}" ]; then
        echo "YOLAB_OUTPUT {{ include "yolab-stack.outputKey" (list . "url") }} https://$YOLAB_FQDN"
      fi
  volumeMounts:
    - name: yolab
      mountPath: /yolab
{{- end }}
{{- end -}}
