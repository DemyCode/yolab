{{- define "yolab-common.tree.root" -}}/tree{{- end -}}


{{- define "yolab-common.instance" -}}
{{- trimPrefix "yolab-" .Release.Namespace -}}
{{- end -}}


{{- define "yolab-common.home" -}}
{{- $instance := include "yolab-common.instance" . -}}
{{- if .Member -}}
{{- printf "%s.%s" $instance .Member -}}
{{- else -}}
{{- $instance -}}
{{- end -}}
{{- end -}}


{{- define "yolab-common.tree.path" -}}
{{- $raw := toString . -}}
{{- $path := trimAll "/" $raw -}}
{{- if or (not $path) (contains "//" $path) (regexMatch "(^|/)\\.{1,2}(/|$)" $path) -}}
{{- fail (printf "%q is not a folder in the file tree" $raw) -}}
{{- end -}}
{{- $path -}}
{{- end -}}


{{- define "yolab-common.tree.volume" -}}
{{- $ctx := index . 0 -}}
{{- $claim := index . 1 -}}
{{- $path := include "yolab-common.tree.path" (index . 2) -}}
{{- $pv := printf "%s.%s" $ctx.Release.Namespace $claim -}}
apiVersion: v1
kind: PersistentVolume
metadata:
  name: {{ $pv }}
  labels:
    yolab.io/tree: "true"
  annotations:
    yolab.io/tree-path: {{ $path | quote }}
spec:
  accessModes:
    - ReadWriteMany
  capacity:
    storage: {{ include "yolab-common.storageSize" $ctx | quote }}
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  volumeMode: Filesystem
  claimRef:
    namespace: {{ $ctx.Release.Namespace }}
    name: {{ $claim }}
  csi:
    driver: cephfs.csi.ceph.com
    volumeHandle: {{ $pv }}
    nodeStageSecretRef:
      name: yolab-cephfs-static
      namespace: ceph-csi
    volumeAttributes:
      clusterID: yolab
      fsName: yolab-fs
      staticVolume: "true"
      rootPath: {{ printf "%s/%s" (include "yolab-common.tree.root" $ctx) $path | quote }}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {{ $claim }}
  namespace: {{ $ctx.Release.Namespace }}
  labels:
    yolab.io/tree: "true"
  annotations:
    yolab.io/tree-path: {{ $path | quote }}
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: ""
  volumeName: {{ $pv }}
  resources:
    requests:
      storage: {{ include "yolab-common.storageSize" $ctx | quote }}
{{- end -}}


{{- define "yolab-common.home.claim" -}}
{{- printf "%s-data" .Release.Name -}}
{{- end -}}


{{- define "yolab-common.homeVolume" -}}
{{ include "yolab-common.tree.volume" (list . (include "yolab-common.home.claim" .) (include "yolab-common.home" .)) }}
{{- end -}}
