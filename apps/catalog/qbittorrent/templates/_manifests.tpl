{{- define "qbittorrent.login" -}}
{{- $cfg := .Values.config -}}
- name: login
  image: python:3.15-alpine@sha256:2699b90f4d562a22e2dd076c82f964032f64814204bff36d271a54ba4cc2c7e2
  imagePullPolicy: IfNotPresent
  env:
    - name: PASSWORD
      value: {{ $cfg.webui_password | quote }}
    - name: SAVE_PATH
      value: {{ with $cfg.media_folder }}{{ printf "/data/%s/downloads" . | quote }}{{ else }}"/downloads"{{ end }}
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
    {{- include "qbittorrent.mounts" . | nindent 4 }}
{{- end -}}


{{- define "qbittorrent.loginOff" -}}
- name: login-off
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      echo "YOLAB_OUTPUT {{ include "yolab-stack.outputKey" (list . "username") }} NONE"
      echo "YOLAB_OUTPUT {{ include "yolab-stack.outputKey" (list . "password") }} NONE"
{{- end -}}


{{- define "qbittorrent.mounts" -}}
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
{{- end -}}


{{- define "qbittorrent.container" -}}
- name: qbittorrent
  image: ghcr.io/linuxserver/qbittorrent:latest@sha256:1ea873647c43d566f251a7122aee330b184509c57ee3257ed19f62dac0f8f24b
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
    {{- include "qbittorrent.mounts" . | nindent 4 }}
{{- end -}}


{{- define "qbittorrent.folderVolume" -}}
{{- with .Values.config.media_folder }}
- name: folder
  persistentVolumeClaim:
    claimName: folder-{{ . }}
{{- end }}
{{- end -}}


{{- define "qbittorrent.manifests" -}}
{{- $prefix := include "yolab-stack.prefix" . -}}
{{- $cfg := .Values.config -}}
{{- $vpn := $cfg.vpn_enabled -}}
{{- if $vpn }}
{{- $gateway := deepCopy (((.Values.yolab).gateway) | default dict) -}}
{{- $_ := set $gateway "upstream" "qbittorrent:8080" -}}
{{- $_ := set .Values.yolab "gateway" $gateway -}}
{{- end }}
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
      storage: {{ $cfg.storage_size | default "200Gi" | quote }}
{{ include "yolab-stack.ownFolderClaim" (list . $cfg.media_folder) }}
---
apiVersion: v1
kind: Service
metadata:
  name: qbittorrent
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}{{ if $vpn }}downloader{{ else }}gateway{{ end }}
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
        {{- include "yolab-stack.podLabels" . | nindent 8 }}
    spec:
      initContainers:
      {{- include "yolab-common.wgRegisterInit" . | nindent 6 }}
      {{- include "yolab-stack.urlOutput" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerInit" . | nindent 6 }}
      {{- if and $cfg.webui_password (not $vpn) }}
      {{- include "qbittorrent.login" . | nindent 6 }}
      {{- end }}
      {{- if not $cfg.webui_password }}
      {{- include "qbittorrent.loginOff" . | nindent 6 }}
      {{- end }}
      containers:
      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerContainer" . | nindent 6 }}
      {{- if not $vpn }}
      {{- include "qbittorrent.container" . | nindent 6 }}
      {{- end }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- include "qbittorrent.folderVolume" . | nindent 6 }}
{{- if $vpn }}
---
apiVersion: v1
kind: Secret
metadata:
  name: {{ .Release.Name }}-vpn
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  private-key: {{ required "a Mullvad WireGuard private key is needed to download through the VPN" $cfg.vpn_private_key | quote }}
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: {{ $prefix }}downloader
  namespace: {{ .Release.Namespace }}
spec:
  replicas: 1
  strategy:
    type: Recreate
  selector:
    matchLabels:
      app: {{ $prefix }}downloader
  template:
    metadata:
      labels:
        app: {{ $prefix }}downloader
        {{- with .Member }}
        yolab.io/member: {{ . }}
        {{- end }}
    spec:
      {{- if $cfg.webui_password }}
      initContainers:
      {{- include "qbittorrent.login" . | nindent 6 }}
      {{- end }}
      containers:
      - name: gluetun
        image: qmcgaw/gluetun:v3@sha256:fa19cc76b2af13d57a8d3dc3066f2ada061b1c761b8aecf989b3877c0486e027
        imagePullPolicy: IfNotPresent
        securityContext:
          capabilities:
            add:
              - NET_ADMIN
        env:
          - name: VPN_SERVICE_PROVIDER
            value: mullvad
          - name: VPN_TYPE
            value: wireguard
          - name: WIREGUARD_PRIVATE_KEY
            valueFrom:
              secretKeyRef:
                name: {{ .Release.Name }}-vpn
                key: private-key
          - name: WIREGUARD_ADDRESSES
            value: {{ required "the Mullvad WireGuard address is needed to download through the VPN" $cfg.vpn_addresses | quote }}
          {{- with $cfg.vpn_server_countries }}
          - name: SERVER_COUNTRIES
            value: {{ . | quote }}
          {{- end }}
          - name: FIREWALL_INPUT_PORTS
            value: "8080"
          - name: FIREWALL_OUTBOUND_SUBNETS
            value: "10.42.0.0/16,10.43.0.0/16,fd00:42::/56,fd00:43::/112"
      {{- include "qbittorrent.container" . | nindent 6 }}
      volumes:
      - name: data
        persistentVolumeClaim:
          claimName: {{ .Release.Name }}-data
      {{- include "qbittorrent.folderVolume" . | nindent 6 }}
{{- end }}
{{- end -}}
