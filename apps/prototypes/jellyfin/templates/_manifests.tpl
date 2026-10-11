{{- define "jellyfin.manifests" -}}
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
spec:
  accessModes:
  - ReadWriteMany
  storageClassName: yolab-cephfs
  resources:
    requests:
      storage: 50Gi
{{ include "yolab-stack.ownFolderClaim" . }}
---
apiVersion: v1
kind: Service
metadata:
  name: jellyfin
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 8096
{{- $gpu := include "jellyfin.gpu" . | fromJson }}
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
        app: {{ $prefix }}gateway
    spec:
      {{- if $gpu.name }}
      nodeSelector:
        kubernetes.io/hostname: {{ $gpu.name | quote }}
      {{- end }}
      initContainers:
      {{- include "yolab-common.wgRegisterInit" . | nindent 6 }}
      {{- include "yolab-stack.urlOutput" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerInit" . | nindent 6 }}
      {{- include "yolab-common.privateAccessInit" . | nindent 6 }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerContainer" . | nindent 6 }}
      {{- include "yolab-common.privateAccessContainers" . | nindent 6 }}
      - name: jellyfin
        image: lscr.io/linuxserver/jellyfin:latest@sha256:1bb4f88d822a0510bb3604b68aacb80d77c1d2a30181e500090c65111b98723b
        imagePullPolicy: IfNotPresent
        readinessProbe:
          httpGet:
            path: /health
            port: 8096
          initialDelaySeconds: 20
          periodSeconds: 10
        command:
        - /bin/sh
        - -c
        - |
          . /yolab/env
          export JELLYFIN_PublishedServerUrl="https://$YOLAB_FQDN"
          exec /init
        env:
        - name: PUID
          value: '1000'
        - name: PGID
          value: '1000'
        - name: TZ
          value: Etc/UTC
        {{- if eq ($gpu.accelerator | default "") "nvidia" }}
        - name: NVIDIA_VISIBLE_DEVICES
          value: all
        - name: NVIDIA_DRIVER_CAPABILITIES
          value: all
        {{- end }}
        {{- if $gpu.accelerator }}
        resources:
          limits:
            {{ if eq $gpu.accelerator "nvidia" }}nvidia.com/gpu-all{{ else }}yolab.io/dri{{ end }}: "1"
        {{- end }}
        volumeMounts:
        - name: data
          mountPath: /config
          subPath: {{ .Release.Name }}/jellyfin/config
        - name: data
          mountPath: /media
          subPath: {{ .Release.Name }}/jellyfin/media
        - name: yolab
          mountPath: /yolab
        {{- with .Values.config.media_folder }}
        - name: folder
          mountPath: /data/{{ . }}
          readOnly: true
        {{- end }}
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
