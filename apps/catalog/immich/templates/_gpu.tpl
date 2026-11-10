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
  "nvidia" "ghcr.io/immich-app/immich-machine-learning:release-cuda@sha256:2c2721c54c5e245b095eea3eb0cda743d112a3f537555b39e2e9a112879a50c1"
  "amd" "ghcr.io/immich-app/immich-machine-learning:release-rocm@sha256:03b4105bdd486a08c762a5da7cb9432cf0b0cd5d5309c9180a8d88f723218be9"
  "intel" "ghcr.io/immich-app/immich-machine-learning:release-openvino@sha256:035c30de002da9d4d4ae3872107bb9d8e195ead1c1f9fd470a2b0149ed37aec1" -}}
{{- get $images (.accelerator | default "") | default "ghcr.io/immich-app/immich-machine-learning:release@sha256:14367bfb374b8576ddf49abd67c5ca0190a4c7fcf6303cdad65d7f7bf1a50bc2" -}}
{{- end -}}

{{- define "immich.mlDevice" -}}
{{- get (dict "nvidia" "nvidia.com/gpu-all" "amd" "yolab.io/kfd" "intel" "yolab.io/dri") (.accelerator | default "") -}}
{{- end -}}
