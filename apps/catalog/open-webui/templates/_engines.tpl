{{- define "open-webui.machines" -}}
{{- $machines := list -}}
{{- if .Values.machines -}}
{{- $machines = .Values.machines -}}
{{- else -}}
{{- range ((lookup "v1" "Node" "" "").items | default list) -}}
{{- $labels := .metadata.labels | default dict -}}
{{- $machines = append $machines (dict "name" .metadata.name "accelerator" (get $labels "yolab.io/accelerator" | default "cpu")) -}}
{{- end -}}
{{- end -}}
{{- toJson $machines -}}
{{- end -}}

{{- define "open-webui.engines" -}}
{{- $engines := list -}}
{{- range (include "open-webui.machines" . | fromJsonArray) -}}
{{- $slug := regexReplaceAll "[^a-z0-9-]" (lower .name) "-" | trunc 40 | trimSuffix "-" -}}
{{- $engine := dict "machine" .name "slug" $slug "accelerator" .accelerator -}}
{{- if eq .accelerator "nvidia" -}}
{{- $engines = append $engines (merge $engine (dict "image" "ollama/ollama:0.35.1@sha256:292ee7945dfc3d5840a181f3ab86fedb1e66703e02c8af98b50f4da56b7e278c" "device" "nvidia.com/gpu-all")) -}}
{{- else if eq .accelerator "amd" -}}
{{- $engines = append $engines (merge $engine (dict "image" "ollama/ollama:0.35.1-rocm@sha256:c716013d3bbf1753ad82cfa0ccdb8bd3252afc6cc16ec37330e49d829560dd4f" "device" "yolab.io/kfd")) -}}
{{- else if and (eq .accelerator "intel") $.Values.vulkanImage -}}
{{- $engines = append $engines (merge $engine (dict "image" $.Values.vulkanImage "device" "yolab.io/dri")) -}}
{{- end -}}
{{- end -}}
{{- toJson $engines -}}
{{- end -}}

{{- define "open-webui.ollamaUrls" -}}
{{- $urls := list -}}
{{- range (include "open-webui.engines" . | fromJsonArray) -}}
{{- $urls = append $urls (printf "http://ollama-%s:11434" .slug) -}}
{{- end -}}
{{- join ";" $urls -}}
{{- end -}}
