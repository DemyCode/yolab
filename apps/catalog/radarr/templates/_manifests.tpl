{{- define "radarr.manifests" -}}
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
  name: radarr
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 7878
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
      - name: radarr
        image: linuxserver/radarr:version-6.3.0.10514@sha256:fe051413dfd91b383ba04910552e83fb0306895fefee275c48dafe7bb90807dc
        imagePullPolicy: IfNotPresent
        env:
          - name: PUID
            value: "1000"
          - name: PGID
            value: "1000"
          - name: TZ
            value: "Etc/UTC"
          {{- include "yolab-stack.servarrKeyEnv" (list . "radarr") | nindent 10 }}
        readinessProbe:
          tcpSocket:
            port: 7878
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
      {{- include "yolab-stack.servarr" (list . (dict "app" "radarr" "port" 7878 "api" "v3" "library" "movies" "category" "movieCategory" "configSubPath" (printf "%s/config" .Release.Name))) | nindent 6 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- with .Values.config.media_folder }}
      - name: folder
        persistentVolumeClaim:
          claimName: folder-{{ . }}
      {{- end }}
{{- end -}}
