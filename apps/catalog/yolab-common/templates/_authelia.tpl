{{- define "yolab-common.auth.enabled" -}}
{{- if (((.Values.config).auth_enabled)) -}}true{{- end -}}
{{- end -}}

{{- define "yolab-common.oidc.ownPortal" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and $cfg.oidc_enabled (eq ($cfg.auth_mode | default "own") "own") -}}true{{- end -}}
{{- end -}}

{{- define "yolab-common.auth.skipGate" -}}
{{- $open := (((.Values.yolab).auth).openPaths) | default list -}}
{{- range $open -}}
{{- if not (regexMatch "^/[A-Za-z0-9._~/*-]*$" .) -}}
{{- fail (printf "yolab.auth.openPaths: %q is not a path matcher" .) -}}
{{- end -}}
{{- end -}}
{{- if $open }}
@yolab_app_checks_itself path {{ join " " $open }}
handle @yolab_app_checks_itself {
  request_header -Remote-User
  request_header -Remote-Groups
  request_header -Remote-Email
  request_header -Remote-Name
  {{- include "yolab-common.reverseProxy" (list . (list)) | nindent 2 }}
}
{{- end }}
{{- if (((.Values.yolab).auth).openWhenAuthorized) }}
@yolab_brings_credentials header Authorization *
handle @yolab_brings_credentials {
  request_header -Remote-User
  request_header -Remote-Groups
  request_header -Remote-Email
  request_header -Remote-Name
  {{- include "yolab-common.reverseProxy" (list . (list)) | nindent 2 }}
}
{{- end }}
{{- if eq (include "yolab-common.apiKey.enabled" .) "true" }}
@yolab_api_key header Authorization "Bearer {$YOLAB_API_KEY}"
handle @yolab_api_key {
  request_header -Remote-User
  request_header -Remote-Groups
  request_header -Remote-Email
  request_header -Remote-Name
  {{- include "yolab-common.reverseProxy" (list . (list)) | nindent 2 }}
}
{{- end }}
{{- end -}}
