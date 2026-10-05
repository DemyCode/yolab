{{- define "immich.gpu" -}}
{{- if not (.Values.config.hardware_acceleration | default false) -}}
{{- toJson dict -}}
{{- else if .Values.gpu.name -}}
{{- toJson .Values.gpu -}}
{{- else -}}
{{- $best := dict -}}
{{- $rank := dict "nvidia" 3 "amd" 2 "intel" 1 -}}
{{- $label := dict "nvidia" "yolab.io/gpu-nvidia" "amd" "yolab.io/gpu-amd" "intel" "yolab.io/gpu-intel-compute" -}}
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
  "nvidia" "ghcr.io/immich-app/immich-machine-learning:release-cuda@sha256:b9fdebfe7f07ff71f77e9d67d509d83c5e055da486a07c12080669c82a80c65e"
  "amd" "ghcr.io/immich-app/immich-machine-learning:release-rocm@sha256:5be1881160b2fe976f5e3fca28b1b88f58ed7031fb391378fd9d955bc400a7db"
  "intel" "ghcr.io/immich-app/immich-machine-learning:release-openvino@sha256:ee28c9419670b7f4c17765fb089527eff977deb8e8fb83e714aa1fa57aa89736" -}}
{{- get $images (.accelerator | default "") | default "ghcr.io/immich-app/immich-machine-learning:release@sha256:e16c2f166a8174901959fdf85e2e4c7bd1ebc4b37e0b6655de97c41408a260c4" -}}
{{- end -}}

{{- define "immich.mlDevice" -}}
{{- get (dict "nvidia" "nvidia.com/gpu-all" "amd" "yolab.io/kfd" "intel" "yolab.io/dri") (.accelerator | default "") -}}
{{- end -}}
