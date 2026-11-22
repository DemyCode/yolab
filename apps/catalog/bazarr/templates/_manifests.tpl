{{- define "bazarr.manifests" -}}
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
  name: bazarr
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 6767
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
      {{- include "yolab-common.privateAccessInit" . | nindent 6 }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.privateAccessContainers" . | nindent 6 }}
      - name: server
        image: linuxserver/bazarr:1.6.2@sha256:e0c5d45965242830c5eeddaafe9ae2b5e8ca3f6c01ecda9ee9364f550921dc50
        imagePullPolicy: IfNotPresent
        env:
        - name: PUID
          value: '1000'
        - name: PGID
          value: '1000'
        ports:
        - containerPort: 6767
        readinessProbe:
          timeoutSeconds: 5
          tcpSocket:
            port: 6767
        volumeMounts:
        - name: data
          mountPath: /config
          subPath: 'data/config'
        {{- include "yolab-common.filesMount" $ | nindent 8 }}
      {{- include "bazarr.configure" . | nindent 6 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.privateAccessVolumes" . | nindent 6 }}
      {{- include "yolab-common.filesVolume" $ | nindent 6 }}
{{- end -}}
