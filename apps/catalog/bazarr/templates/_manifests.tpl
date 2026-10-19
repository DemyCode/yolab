{{- define "bazarr.manifests" -}}
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
      storage: {{ $cfg.storage_size | default "10Gi" | quote }}
{{ include "yolab-stack.ownFolderClaim" (list . $cfg.media_folder) }}
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
      {{- include "yolab-common.fileExplorerInit" . | nindent 6 }}
      {{- include "yolab-common.privateAccessInit" . | nindent 6 }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerContainer" . | nindent 6 }}
      {{- include "yolab-common.privateAccessContainers" . | nindent 6 }}
      - name: server
        image: linuxserver/bazarr:1.6.2@sha256:fb0437897eaa7f96cf6ac42f795f8dcc208d34ec969eac5d366454e767304eb9
        imagePullPolicy: IfNotPresent
        env:
        - name: PUID
          value: '1000'
        - name: PGID
          value: '1000'
        ports:
        - containerPort: 6767
        readinessProbe:
          tcpSocket:
            port: 6767
        volumeMounts:
        - name: data
          mountPath: /config
          subPath: '{{ .Release.Name }}/data/config'
        - name: data
          mountPath: /downloads
          subPath: '{{ .Release.Name }}/storage/downloads'
        {{- with $cfg.media_folder }}
        - name: folder
          mountPath: /data/{{ . }}
        {{- end }}
      {{- include "bazarr.configure" . | nindent 6 }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- include "yolab-common.privateAccessVolumes" . | nindent 6 }}
      {{- with $cfg.media_folder }}
      - name: folder
        persistentVolumeClaim:
          claimName: folder-{{ . }}
      {{- end }}
{{- end -}}
