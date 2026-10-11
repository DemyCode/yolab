{{- define "navidrome.manifests" -}}
{{- $prefix := include "yolab-stack.prefix" . -}}
{{- $cfg := .Values.config -}}
{{ include "yolab-common.caddyConfigMap" . }}
---
{{ include "yolab-common.fileExplorerSecret" . }}
---
{{ include "yolab-common.fileExplorerConfigMap" . }}
---
{{ include "yolab-common.privateAccessResources" . }}
---
{{ include "yolab-common.uninstallHook" . }}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: {{ .Release.Name }}-data
  namespace: {{ .Release.Namespace }}
  {{- include "yolab-stack.keep" . | nindent 2 }}
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: yolab-cephfs
  resources:
    requests:
      storage: {{ $cfg.storage_size | default "100Gi" | quote }}
{{ include "yolab-stack.ownFolderClaim" (list . $cfg.music_folder) }}
---
apiVersion: v1
kind: Service
metadata:
  name: navidrome
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 4533
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ $prefix }}gateway
  namespace: {{ .Release.Namespace }}
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: {{ $prefix }}gateway
  template:
    metadata:
      labels:
        {{- include "yolab-stack.podLabels" . | nindent 8 }}
    spec:
      initContainers:
      {{- include "yolab-common.wgRegisterInit" . | nindent 6 }}
      {{- include "yolab-stack.urlOutput" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerInit" . | nindent 6 }}
      {{- include "yolab-common.privateAccessInit" . | nindent 6 }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerContainer" . | nindent 6 }}
      {{- include "yolab-common.privateAccessContainers" . | nindent 6 }}
      - name: navidrome
        image: deluan/navidrome:latest@sha256:38dc2727bfcfd5ede290f8ada114fc90368146f265ae4701ddddbcbe2a44ee52
        imagePullPolicy: IfNotPresent
        env:
          - name: ND_MUSICFOLDER
            {{- if $cfg.music_folder }}
            value: {{ trimSuffix "/" (printf "/data/%s/%s" $cfg.music_folder ($cfg.music_path | default "")) | quote }}
            {{- else }}
            value: "/music"
            {{- end }}
          - name: ND_DATAFOLDER
            value: "/data"
        readinessProbe:
          tcpSocket:
            port: 4533
          initialDelaySeconds: 10
          periodSeconds: 10
        volumeMounts:
          - name: data
            mountPath: /data
            subPath: {{ .Release.Name }}/navidrome
          - name: data
            mountPath: /music
            subPath: {{ .Release.Name }}/music
          {{- with $cfg.music_folder }}
          - name: folder
            mountPath: /data/{{ . }}
            readOnly: true
          {{- end }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- include "yolab-common.privateAccessVolumes" . | nindent 6 }}
      {{- with $cfg.music_folder }}
      - name: folder
        persistentVolumeClaim:
          claimName: folder-{{ . }}
      {{- end }}
{{- end -}}
