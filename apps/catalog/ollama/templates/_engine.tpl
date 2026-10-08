{{- define "ollama.machine" -}}
{{- if .Values.machine.name -}}
{{- toJson .Values.machine -}}
{{- else -}}
{{- $best := dict -}}
{{- $rank := dict "nvidia" 4 "amd" 3 "vulkan" 2 "intel" 1 -}}
{{- range ((lookup "v1" "Node" "" "").items | default list) -}}
{{- $accelerator := get (.metadata.labels | default dict) "yolab.io/accelerator" -}}
{{- if and (hasKey $rank $accelerator) (gt (int (get $rank $accelerator)) (int (get $rank ($best.accelerator | default "none") | default 0))) -}}
{{- $best = dict "name" .metadata.name "accelerator" $accelerator -}}
{{- end -}}
{{- end -}}
{{- toJson $best -}}
{{- end -}}
{{- end -}}

{{- define "ollama.engine" -}}
{{- $engines := dict
  "nvidia" (dict "image" .Values.images.cuda "device" "nvidia.com/gpu-all")
  "amd" (dict "image" .Values.images.rocm "device" "yolab.io/kfd" "x86Only" true)
  "vulkan" (dict "image" .Values.images.vulkan "device" "yolab.io/dri" "x86Only" true)
  "intel" (dict "image" .Values.images.vulkan "device" "yolab.io/dri" "x86Only" true) -}}
{{- $machine := include "ollama.machine" . | fromJson -}}
{{- $engine := get $engines ($machine.accelerator | default "") | default (dict "image" .Values.images.cuda) -}}
{{- toJson (merge (dict "machine" ($machine.name | default "")) $engine) -}}
{{- end -}}
