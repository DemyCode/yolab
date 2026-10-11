{{- define "jellyseerr.manifests" -}}
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
      storage: {{ $cfg.storage_size | default "5Gi" | quote }}
---
apiVersion: v1
kind: Service
metadata:
  name: jellyseerr
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 5055
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
      - name: config-owner
        image: ghcr.io/seerr-team/seerr:v3.5.0@sha256:27602401178d54f1964442287b9f23f67a3fa2252645ee8065839ea1c3f69e45
        imagePullPolicy: IfNotPresent
        securityContext:
          runAsUser: 0
          runAsGroup: 0
        command:
          - chown
          - -R
          - 1000:1000
          - /app/config
        volumeMounts:
          - name: data
            mountPath: /app/config
            subPath: {{ .Release.Name }}/jellyseerr
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerContainer" . | nindent 6 }}
      {{- include "yolab-common.privateAccessContainers" . | nindent 6 }}
      - name: seerr
        image: ghcr.io/seerr-team/seerr:v3.5.0@sha256:27602401178d54f1964442287b9f23f67a3fa2252645ee8065839ea1c3f69e45
        imagePullPolicy: IfNotPresent
        readinessProbe:
          tcpSocket:
            port: 5055
          initialDelaySeconds: 10
          periodSeconds: 10
        volumeMounts:
          - name: data
            mountPath: /app/config
            subPath: {{ .Release.Name }}/jellyseerr
      {{- include "jellyseerr.configure" . | nindent 6 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- include "yolab-common.privateAccessVolumes" . | nindent 6 }}
{{- end -}}
