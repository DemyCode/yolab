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
  "nvidia" "ghcr.io/immich-app/immich-machine-learning:release-cuda@sha256:d1a195d377cfac65886901a6b2d4040b61b7e7db7ca55db1598622d3a2f125e2"
  "amd" "ghcr.io/immich-app/immich-machine-learning:release-rocm@sha256:0936fa0414903164315c9c80a9f2c2a6ed9b8387ce05decbfe606d1d54e7a2ca"
  "intel" "ghcr.io/immich-app/immich-machine-learning:release-openvino@sha256:9f5ea923b763435592280e2f5e051a62f1a33d6d14e87620fc15453c27a12ae0" -}}
{{- get $images (.accelerator | default "") | default "ghcr.io/immich-app/immich-machine-learning:release@sha256:513c831cfb010ad319341a0c86b42c575a0d5b688d3e8cae346ee624074a58ed" -}}
{{- end -}}

{{- define "immich.mlDevice" -}}
{{- get (dict "nvidia" "nvidia.com/gpu-all" "amd" "yolab.io/kfd" "intel" "yolab.io/dri") (.accelerator | default "") -}}
{{- end -}}
