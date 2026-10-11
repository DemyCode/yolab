{{- define "qbittorrent.password" -}}
{{- $existing := lookup "v1" "Secret" .Release.Namespace (printf "%s-login" .Release.Name) -}}
{{- if .Values.config.webui_password -}}
{{- .Values.config.webui_password -}}
{{- else if and $existing $existing.data (index $existing.data "password") -}}
{{- index $existing.data "password" | b64dec -}}
{{- else -}}
{{- randAlphaNum 24 -}}
{{- end -}}
{{- end -}}


{{- define "qbittorrent.manifests" -}}
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
kind: Secret
metadata:
  name: {{ .Release.Name }}-login
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  username: admin
  password: {{ include "qbittorrent.password" . | quote }}
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
      storage: {{ .Values.config.storage_size | default "200Gi" | quote }}
{{ include "yolab-stack.ownFolderClaim" . }}
---
apiVersion: v1
kind: Service
metadata:
  name: qbittorrent
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 8080
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
      initContainers:
      {{- include "yolab-common.wgRegisterInit" . | nindent 6 }}
      {{- include "yolab-stack.urlOutput" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerInit" . | nindent 6 }}
      - name: login
        image: python:3.14-alpine@sha256:f6a589d43c42b9e7f7dc67a12d37132491f362859a5d750607710cc56da3bc72
        imagePullPolicy: IfNotPresent
        env:
          - name: PASSWORD
            valueFrom:
              secretKeyRef:
                name: {{ .Release.Name }}-login
                key: password
          - name: SAVE_PATH
            value: {{ with .Values.config.media_folder }}{{ printf "/data/%s/downloads" . | quote }}{{ else }}"/downloads"{{ end }}
          - name: USERNAME_OUTPUT
            value: {{ include "yolab-stack.outputKey" (list . "username") | quote }}
          - name: PASSWORD_OUTPUT
            value: {{ include "yolab-stack.outputKey" (list . "password") | quote }}
        command:
          - python3
          - -c
          - |
            import base64, configparser, hashlib, os
            path = "/config/qBittorrent/qBittorrent.conf"
            os.makedirs(os.path.dirname(path), exist_ok=True)
            conf = configparser.RawConfigParser(strict=False)
            conf.optionxform = str
            conf.read(path)
            for section in ("Preferences", "BitTorrent", "LegalNotice"):
                if not conf.has_section(section):
                    conf.add_section(section)
            password = os.environ["PASSWORD"]
            salt = os.urandom(16)
            key = hashlib.pbkdf2_hmac("sha512", password.encode(), salt, 100000, 64)
            hashed = "%s:%s" % (base64.b64encode(salt).decode(), base64.b64encode(key).decode())
            conf.set("Preferences", "WebUI\\Username", "admin")
            conf.set("Preferences", "WebUI\\Password_PBKDF2", '"@ByteArray(%s)"' % hashed)
            conf.set("LegalNotice", "Accepted", "true")
            save = os.environ["SAVE_PATH"]
            os.makedirs(save, exist_ok=True)
            os.chown(save, 1000, 1000)
            conf.set("BitTorrent", "Session\\DefaultSavePath", save)
            with open(path, "w") as f:
                conf.write(f, space_around_delimiters=False)
            for p in (os.path.dirname(path), path):
                os.chown(p, 1000, 1000)
            print("YOLAB_OUTPUT %s admin" % os.environ["USERNAME_OUTPUT"])
            print("YOLAB_OUTPUT %s %s" % (os.environ["PASSWORD_OUTPUT"], password))
        volumeMounts:
          - name: data
            mountPath: /config
            subPath: {{ .Release.Name }}/config
          - name: data
            mountPath: /downloads
            subPath: {{ .Release.Name }}/downloads
          {{- with .Values.config.media_folder }}
          - name: folder
            mountPath: /data/{{ . }}
          {{- end }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerContainer" . | nindent 6 }}
      - name: qbittorrent
        image: ghcr.io/linuxserver/qbittorrent:latest@sha256:b522f9f4b769f8f36d49d22d5eb6a92e9aa18904c6a1830b1439df511ec21983
        imagePullPolicy: IfNotPresent
        env:
          - name: PUID
            value: "1000"
          - name: PGID
            value: "1000"
          - name: TZ
            value: "Etc/UTC"
          - name: WEBUI_PORT
            value: "8080"
        readinessProbe:
          tcpSocket:
            port: 8080
          initialDelaySeconds: 10
          periodSeconds: 10
        volumeMounts:
          - name: data
            mountPath: /config
            subPath: {{ .Release.Name }}/config
          - name: data
            mountPath: /downloads
            subPath: {{ .Release.Name }}/downloads
          {{- with .Values.config.media_folder }}
          - name: folder
            mountPath: /data/{{ . }}
          {{- end }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- with .Values.config.media_folder }}
      - name: folder
        persistentVolumeClaim:
          claimName: folder-{{ . }}
      {{- end }}
{{- end -}}
