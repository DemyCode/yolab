{{- define "immich.gpu" -}}
{{- if not (.Values.config.hardware_acceleration | default false) -}}
{{- toJson dict -}}
{{- else if .Values.gpu.name -}}
{{- toJson .Values.gpu -}}
{{- else -}}
{{- $best := dict -}}
{{- $rank := dict "nvidia" 3 "amd" 2 "intel" 1 -}}
{{- $label := dict "nvidia" "yolab.io/gpu-nvidia" "amd" "yolab.io/gpu-amd-rocm" "intel" "yolab.io/gpu-intel-compute" -}}
{{- range ((lookup "v1" "Node" "" "").items | default list) -}}
{{- $labels := .metadata.labels | default dict -}}
{{- if eq (get $labels "kubernetes.io/arch") "amd64" -}}
{{- $node := .metadata.name -}}
{{- range $vendor := list "nvidia" "amd" "intel" -}}
{{- if and (eq (get $labels (get $label $vendor)) "true") (gt (int (get $rank $vendor)) (int (get $rank ($best.accelerator | default "none") | default 0))) -}}
{{- $best = dict "name" $node "accelerator" $vendor -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- end -}}
{{- toJson $best -}}
{{- end -}}
{{- end -}}

{{- define "immich.mlImage" -}}
{{- $images := dict
  "nvidia" "ghcr.io/immich-app/immich-machine-learning:release-cuda@sha256:1bbdd544178fbd5702fba7f2932505a78ec98d6a0f90e0ae87caaa2696228f5c"
  "amd" "ghcr.io/immich-app/immich-machine-learning:release-rocm@sha256:cea1ff8726bffa6ebe8b08607ccafd996670965aeb195dbbdcdc6e0ac93ea15b"
  "intel" "ghcr.io/immich-app/immich-machine-learning:release-openvino@sha256:43b6b964c6cc458b624ec9b7ed2af0231e19e3bcfaaa68fba8a2668353c48251" -}}
{{- get $images (.accelerator | default "") | default "ghcr.io/immich-app/immich-machine-learning:release@sha256:73915f07e1f4289ba45b174c58443db74cef47ddf502613f475fc5e52458ea42" -}}
{{- end -}}

{{- define "immich.mlDevice" -}}
{{- get (dict "nvidia" "nvidia.com/gpu-all" "amd" "yolab.io/kfd" "intel" "yolab.io/dri") (.accelerator | default "") -}}
{{- end -}}
