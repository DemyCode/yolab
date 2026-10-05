{{- define "jellyfin.gpu" -}}
{{- if not (.Values.config.hardware_transcoding | default false) -}}
{{- toJson dict -}}
{{- else if .Values.gpu.name -}}
{{- toJson .Values.gpu -}}
{{- else -}}
{{- $best := dict -}}
{{- $rank := dict "intel" 3 "nvidia" 2 "amd" 1 -}}
{{- range ((lookup "v1" "Node" "" "").items | default list) -}}
{{- $labels := .metadata.labels | default dict -}}
{{- $accelerator := get $labels "yolab.io/accelerator" -}}
{{- if and (hasKey $rank $accelerator) (gt (int (get $rank $accelerator)) (int (get $rank ($best.accelerator | default "none") | default 0))) -}}
{{- $best = dict "name" .metadata.name "accelerator" $accelerator -}}
{{- end -}}
{{- end -}}
{{- toJson $best -}}
{{- end -}}
{{- end -}}
