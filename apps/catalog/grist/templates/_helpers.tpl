{{- define "grist.widgetsDigest" -}}
{{- $all := "" -}}
{{- range $path, $_ := .Files.Glob "files/widgets/*" -}}
{{- $all = printf "%s\n%s\n%s" $all $path ($.Files.Get $path) -}}
{{- end -}}
{{- $all | sha256sum | trunc 16 -}}
{{- end -}}
