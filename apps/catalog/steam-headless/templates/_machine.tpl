{{- define "steam-headless.machine" -}}
{{- if .Values.machine.name -}}
{{- toJson .Values.machine -}}
{{- else -}}
{{- $best := dict -}}
{{- $rank := dict "nvidia" 3 "amd" 2 "intel" 1 -}}
{{- range ((lookup "v1" "Node" "" "").items | default list) -}}
{{- $labels := .metadata.labels | default dict -}}
{{- $accelerator := get $labels "yolab.io/accelerator" -}}
{{- if and (eq (get $labels "yolab.io/game-input") "true") (hasKey $rank $accelerator) -}}
{{- if gt (int (get $rank $accelerator)) (int (get $rank ($best.accelerator | default "none") | default 0)) -}}
{{- $best = dict "name" .metadata.name "accelerator" $accelerator -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- toJson $best -}}
{{- end -}}
{{- end -}}

{{- define "steam-headless.device" -}}
{{- $accelerator := .accelerator | default "" -}}
{{- if eq $accelerator "nvidia" -}}nvidia.com/gpu-all{{- else if $accelerator -}}yolab.io/dri{{- end -}}
{{- end -}}
