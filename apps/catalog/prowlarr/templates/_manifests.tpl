{{- define "prowlarr.manifests" -}}
{{- $prefix := include "yolab-stack.prefix" . -}}
{{- $cfg := .Values.config -}}
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
      storage: {{ $cfg.storage_size | default "2Gi" | quote }}
---
apiVersion: v1
kind: Service
metadata:
  name: prowlarr
  namespace: {{ .Release.Namespace }}
spec:
  selector:
    app: {{ $prefix }}gateway
  ports:
    - name: http
      port: 9696
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
      - name: prowlarr
        image: linuxserver/prowlarr:version-2.5.2.5491@sha256:aaa16ba7d66e413db6d0358d3b6c68d833578ba7f176f24a51807181e7637fce
        imagePullPolicy: IfNotPresent
        env:
          - name: PUID
            value: "1000"
          - name: PGID
            value: "1000"
          - name: TZ
            value: "Etc/UTC"
          {{- include "yolab-stack.servarrKeyEnv" (list . "prowlarr") | nindent 10 }}
        readinessProbe:
          tcpSocket:
            port: 9696
          initialDelaySeconds: 15
          periodSeconds: 10
          failureThreshold: 12
        volumeMounts:
          - name: data
            mountPath: /config
            subPath: {{ .Release.Name }}/config
      - name: configure
        image: {{ include "yolab-common.image.wgRegister" . }}
        imagePullPolicy: IfNotPresent
        securityContext:
          runAsUser: 1000
          runAsGroup: 1000
        env:
          - name: API_KEY
            value: {{ $cfg.api_key | default "" | quote }}
          - name: API_KEY_OUTPUT
            value: {{ include "yolab-stack.outputKey" (list . "api_key") | quote }}
          - name: API
            value: "http://localhost:9696/api/v1"
          - name: SELF
            value: {{ printf "http://prowlarr.%s.svc.cluster.local:9696" .Release.Namespace | quote }}
          {{- range $app := list "sonarr" "radarr" "lidarr" }}
          {{- $link := index $cfg $app | default dict }}
          - name: {{ $app | upper }}_URL
            value: {{ $link.internal_url | default "" | quote }}
          - name: {{ $app | upper }}_KEY
            value: {{ $link.api_key | default "" | quote }}
          {{- end }}
        command:
          - /bin/sh
          - -c
          - |
            set -u
            {{- include "yolab-stack.servarrKeyScript" . | nindent 12 }}
            register() {
              [ -n "$2" ] || return 0
              template=$(call "$API/applications/schema" \
                | jq -e --arg i "$1" 'first(.[] | select(.implementation == $i))') || return 1
              body=$(printf '%s' "$template" | jq --arg name "$1 (YoLab)" --arg self "$SELF" \
                --arg url "$2" --arg key "$3" '
                  .name = $name | .syncLevel = "fullSync" | .tags = []
                  | .fields |= map(
                      if .name == "prowlarrUrl" then .value = $self
                      elif .name == "baseUrl" then .value = $url
                      elif .name == "apiKey" then .value = $key
                      else . end)') || return 1
              upsert applications "$1 (YoLab)" "$body"
            }
            until call "$API/system/status" >/dev/null 2>&1; do sleep 5; done
            until register Sonarr "$SONARR_URL" "$SONARR_KEY" \
              && register Radarr "$RADARR_URL" "$RADARR_KEY" \
              && register Lidarr "$LIDARR_URL" "$LIDARR_KEY"; do
              echo "prowlarr is up but not wired yet, retrying in 15s"
              sleep 15
            done
            echo "prowlarr is wired"
            while :; do sleep 3600; done
        volumeMounts:
          - name: data
            mountPath: /config
            subPath: {{ .Release.Name }}/config
            readOnly: true
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
{{- end -}}
