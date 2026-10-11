{{- define "sonarr.manifests" -}}
{{- $prefix := include "yolab-stack.prefix" . -}}
{{ include "yolab-common.caddyConfigMap" . }}
---
{{ include "yolab-common.fileExplorerSecret" . }}
---
{{ include "yolab-common.fileExplorerConfigMap" . }}
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
      storage: {{ .Values.config.storage_size | default "50Gi" | quote }}
{{ include "yolab-stack.ownFolderClaim" (list . .Values.config.media_folder) }}
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
      {{- include "yolab-stack.urlOutput" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerInit" . | nindent 6 }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerContainer" . | nindent 6 }}
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
            subPath: {{ .Release.Name }}/config
          - name: data
            mountPath: /media
            subPath: media
          {{- with .Values.config.media_folder }}
          - name: folder
            mountPath: /data/{{ . }}
          {{- end }}
      {{- include "yolab-stack.servarr" (list . (dict "app" "sonarr" "port" 8989 "api" "v3" "library" "tv" "category" "tvCategory" "configSubPath" (printf "%s/config" .Release.Name))) | nindent 6 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- with .Values.config.media_folder }}
      - name: folder
        persistentVolumeClaim:
          claimName: folder-{{ . }}
      {{- end }}
{{- end -}}
