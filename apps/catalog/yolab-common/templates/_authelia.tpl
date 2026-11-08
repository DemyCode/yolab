{{- define "yolab-common.auth.enabled" -}}
{{- if (((.Values.config).auth_enabled)) -}}true{{- end -}}
{{- end -}}
