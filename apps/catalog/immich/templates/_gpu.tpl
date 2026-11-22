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
  "amd" "ghcr.io/immich-app/immich-machine-learning:release-rocm@sha256:d6910817cb774b1211b2475eb5844af783386eaed00106dea1f0fee9d6184042"
  "intel" "ghcr.io/immich-app/immich-machine-learning:release-openvino@sha256:97f60b06c5938c326d3ba416bcf24bc94d233f34554c05f3b76336f98ffd5ea9" -}}
{{- get $images (.accelerator | default "") | default "ghcr.io/immich-app/immich-machine-learning:release@sha256:739405742a79bd7447227c553bb62ed5491aa36acf00bc667af7f9c93b6fae91" -}}
{{- end -}}

{{- define "immich.mlDevice" -}}
{{- get (dict "nvidia" "nvidia.com/gpu-all" "amd" "yolab.io/kfd" "intel" "yolab.io/dri") (.accelerator | default "") -}}
{{- end -}}
