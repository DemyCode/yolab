{{- define "open-webui.engines" -}}
{{- $engines := list
  (dict "vendor" "nvidia" "image" "ollama/ollama:0.35.1@sha256:292ee7945dfc3d5840a181f3ab86fedb1e66703e02c8af98b50f4da56b7e278c" "device" "nvidia.com/gpu-all")
  (dict "vendor" "amd" "image" "ollama/ollama:0.35.1-rocm@sha256:c716013d3bbf1753ad82cfa0ccdb8bd3252afc6cc16ec37330e49d829560dd4f" "device" "yolab.io/kfd" "x86Only" true) -}}
{{- if .Values.vulkanImage -}}
{{- $engines = append $engines (dict "vendor" "intel" "image" .Values.vulkanImage "device" "yolab.io/dri" "x86Only" true) -}}
{{- end -}}
{{- toJson $engines -}}
{{- end -}}

{{- define "open-webui.accelerators" -}}
{{- $found := list -}}
{{- if .Values.machines -}}
{{- range .Values.machines -}}
{{- $found = append $found (.accelerator | default "cpu") -}}
{{- end -}}
{{- else -}}
{{- range ((lookup "v1" "Node" "" "").items | default list) -}}
{{- $found = append $found (get (.metadata.labels | default dict) "yolab.io/accelerator" | default "cpu") -}}
{{- end -}}
{{- end -}}
{{- toJson $found -}}
{{- end -}}

{{- define "open-webui.onGpu" -}}
{{- $accelerators := include "open-webui.accelerators" . | fromJsonArray -}}
{{- range (include "open-webui.engines" . | fromJsonArray) -}}
{{- if has .vendor $accelerators -}}true{{- end -}}
{{- end -}}
{{- end -}}
