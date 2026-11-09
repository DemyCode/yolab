{{- define "navidrome.manifests" -}}
{{- $prefix := include "yolab-stack.prefix" . -}}
{{- $cfg := .Values.config -}}
{{ include "yolab-common.caddyConfigMap" . }}
---
{{ include "yolab-common.privateAccessResources" . }}
---
{{ include "yolab-common.uninstallHook" . }}
---
{{ include "yolab-common.homeVolume" $ }}
---
{{ include "yolab-common.files" . }}
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
      {{- include "yolab-common.filesDirs" (list . (list $cfg.music_path)) | nindent 6 }}
      {{- include "yolab-stack.urlOutput" . | nindent 6 }}
      {{- include "yolab-common.privateAccessInit" . | nindent 6 }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.privateAccessContainers" . | nindent 6 }}
      - name: navidrome
        image: deluan/navidrome:latest@sha256:38dc2727bfcfd5ede290f8ada114fc90368146f265ae4701ddddbcbe2a44ee52
        imagePullPolicy: IfNotPresent
        env:
          - name: ND_MUSICFOLDER
            value: {{ include "yolab-common.files.path" (list . $cfg.music_path) | quote }}
          - name: ND_DATAFOLDER
            value: "/data"
        readinessProbe:
          timeoutSeconds: 5
          tcpSocket:
            port: 4533
          initialDelaySeconds: 10
          periodSeconds: 10
        volumeMounts:
          - name: data
            mountPath: /data
            subPath: navidrome
          {{- include "yolab-common.filesMount" $ | nindent 10 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.privateAccessVolumes" . | nindent 6 }}
      {{- include "yolab-common.filesVolume" $ | nindent 6 }}
{{- end -}}
