{{- define "lidarr.manifests" -}}
{{- $prefix := include "yolab-stack.prefix" . -}}
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
      storage: {{ .Values.config.storage_size | default "10Gi" | quote }}
{{ include "yolab-stack.ownFolderClaim" (list . .Values.config.media_folder) }}
---
apiVersion: v1
kind: Service
metadata:
  name: lidarr
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 8686
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
      - name: server
        image: linuxserver/lidarr:3.1.0@sha256:67887807b13ae025c0fe979da298347c8609969087ab064c81d8e9acc1e06bbf
        imagePullPolicy: IfNotPresent
        env:
        - name: PUID
          value: '1000'
        - name: PGID
          value: '1000'
        {{- include "yolab-stack.servarrKeyEnv" (list . "lidarr") | nindent 8 }}
        ports:
        - containerPort: 8686
        readinessProbe:
          tcpSocket:
            port: 8686
        volumeMounts:
        - name: data
          mountPath: /config
          subPath: '{{ .Release.Name }}/data/config'
        - name: data
          mountPath: /downloads
          subPath: '{{ .Release.Name }}/storage/downloads'
        {{- with .Values.config.media_folder }}
        - name: folder
          mountPath: /data/{{ . }}
        {{- end }}
      {{- include "yolab-stack.servarr" (list . (dict "app" "lidarr" "port" 8686 "api" "v1" "library" "music" "category" "musicCategory" "configSubPath" (printf "%s/data/config" .Release.Name))) | nindent 6 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- include "yolab-common.privateAccessVolumes" . | nindent 6 }}
      {{- with .Values.config.media_folder }}
      - name: folder
        persistentVolumeClaim:
          claimName: folder-{{ . }}
      {{- end }}
{{- end -}}
