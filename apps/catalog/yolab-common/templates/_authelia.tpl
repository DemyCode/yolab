{{- define "yolab-common.auth.enabled" -}}
{{- if (((.Values.config).auth_enabled)) -}}true{{- end -}}
{{- end -}}

{{- define "yolab-common.oidc.ownPortal" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and $cfg.oidc_enabled (eq ($cfg.auth_mode | default "own") "own") -}}true{{- end -}}
{{- end -}}
