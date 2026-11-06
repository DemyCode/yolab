{{- define "sonarr.manifests" -}}
{{- $prefix := include "yolab-stack.prefix" . -}}
{{ include "yolab-common.caddyConfigMap" . }}
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
  name: sonarr
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 8989
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
      {{- include "yolab-common.filesDirs" (list . (list .Values.config.shows_path)) | nindent 6 }}
      {{- include "yolab-stack.urlOutput" . | nindent 6 }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      - name: sonarr
        image: linuxserver/sonarr:version-4.0.19.2979@sha256:82172b363f9eddc02aca544f880a044f2ceb9aaf190bc87e4454e2f705eb91d0
        imagePullPolicy: IfNotPresent
        env:
          - name: PUID
            value: "1000"
          - name: PGID
            value: "1000"
          - name: TZ
            value: "Etc/UTC"
          {{- include "yolab-stack.servarrKeyEnv" (list . "sonarr") | nindent 10 }}
        readinessProbe:
          tcpSocket:
            port: 8989
          initialDelaySeconds: 15
          periodSeconds: 10
          failureThreshold: 12
        volumeMounts:
          - name: data
            mountPath: /config
            subPath: config
          {{- include "yolab-common.filesMount" $ | nindent 10 }}
      {{- include "yolab-stack.servarr" (list . (dict "app" "sonarr" "port" 8989 "api" "v3" "root" .Values.config.shows_path "category" "tvCategory" "configSubPath" "config")) | nindent 6 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.filesVolume" $ | nindent 6 }}
{{- end -}}
