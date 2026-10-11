{{- define "radarr.apiKey" -}}
{{- $existing := lookup "v1" "Secret" .Release.Namespace (printf "%s-api-key" .Release.Name) -}}
{{- if and $existing $existing.data (index $existing.data "api-key") -}}
{{- index $existing.data "api-key" | b64dec -}}
{{- else -}}
{{- uuidv4 | replace "-" "" -}}
{{- end -}}
{{- end -}}


{{- define "radarr.manifests" -}}
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
kind: Secret
metadata:
  name: {{ .Release.Name }}-api-key
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  api-key: {{ include "radarr.apiKey" . | quote }}
{{- if $cfg.download_client_host }}
---
apiVersion: v1
kind: Secret
metadata:
  name: {{ .Release.Name }}-download-client
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  password: {{ $cfg.download_client_password | default "" | quote }}
{{- end }}
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
      storage: {{ $cfg.storage_size | default "50Gi" | quote }}
{{ include "yolab-stack.ownFolderClaim" . }}
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
        app: {{ $prefix }}gateway
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
          - name: RADARR__AUTH__APIKEY
            valueFrom:
              secretKeyRef:
                name: {{ .Release.Name }}-api-key
                key: api-key
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
          {{- with $cfg.media_folder }}
          - name: folder
            mountPath: /data/{{ . }}
          {{- end }}
      - name: configure
        image: {{ include "yolab-common.image.wgRegister" . }}
        imagePullPolicy: IfNotPresent
        securityContext:
          runAsUser: 1000
          runAsGroup: 1000
        env:
          - name: API_KEY
            valueFrom:
              secretKeyRef:
                name: {{ .Release.Name }}-api-key
                key: api-key
          - name: API_KEY_OUTPUT
            value: {{ include "yolab-stack.outputKey" (list . "api_key") | quote }}
          - name: ROOT_FOLDER
            value: {{ with $cfg.media_folder }}{{ printf "/data/%s/movies" . | quote }}{{ else }}"/media/movies"{{ end }}
          {{- with $cfg.download_client_host }}
          - name: DC_HOST
            value: {{ . | quote }}
          - name: DC_PORT
            value: {{ $cfg.download_client_port | default 8080 | quote }}
          - name: DC_USERNAME
            value: {{ $cfg.download_client_username | default "admin" | quote }}
          - name: DC_PASSWORD
            valueFrom:
              secretKeyRef:
                name: {{ $.Release.Name }}-download-client
                key: password
          {{- end }}
        command:
          - /bin/sh
          - -c
          - |
            set -u
            API=http://localhost:7878/api/v3
            NAME="qBittorrent (YoLab)"
            echo "YOLAB_OUTPUT $API_KEY_OUTPUT $API_KEY"
            call() {
              curl -fsS -H "X-Api-Key: $API_KEY" -H "Content-Type: application/json" "$@"
            }
            root_folder() {
              mkdir -p "$ROOT_FOLDER" || return 1
              call "$API/rootfolder" | jq -e --arg p "$ROOT_FOLDER" \
                'any(.[]; (.path | rtrimstr("/")) == $p)' >/dev/null && return 0
              call -X POST "$API/rootfolder" -d "$(jq -n --arg p "$ROOT_FOLDER" '{path: $p}')" >/dev/null
            }
            download_client() {
              [ -n "${DC_HOST:-}" ] || return 0
              template=$(call "$API/downloadclient/schema" \
                | jq -e 'first(.[] | select(.implementation == "QBittorrent"))') || return 1
              body=$(printf '%s' "$template" | jq --arg name "$NAME" --arg host "$DC_HOST" \
                --argjson port "$DC_PORT" --arg user "$DC_USERNAME" --arg pass "$DC_PASSWORD" '
                  .name = $name | .enable = true | .priority = 1 | .tags = []
                  | .removeCompletedDownloads = true | .removeFailedDownloads = true
                  | .fields |= map(
                      if .name == "host" then .value = $host
                      elif .name == "port" then .value = $port
                      elif .name == "useSsl" then .value = false
                      elif .name == "username" then .value = $user
                      elif .name == "password" then .value = $pass
                      elif .name == "movieCategory" then .value = "radarr"
                      else . end)') || return 1
              id=$(call "$API/downloadclient" | jq -r --arg name "$NAME" 'first(.[] | select(.name == $name) | .id) // empty') || return 1
              if [ -n "$id" ]; then
                call -X PUT "$API/downloadclient/$id?forceSave=true" \
                  -d "$(printf '%s' "$body" | jq --argjson id "$id" '. + {id: $id}')" >/dev/null
              else
                call -X POST "$API/downloadclient?forceSave=true" -d "$body" >/dev/null
              fi
            }
            until call "$API/system/status" >/dev/null 2>&1; do sleep 5; done
            until root_folder && download_client; do
              echo "Radarr is up but not wired yet, retrying in 15s"
              sleep 15
            done
            echo "Radarr is wired: films go to $ROOT_FOLDER${DC_HOST:+, downloads to $DC_HOST:$DC_PORT}"
            exec sleep infinity
        volumeMounts:
          - name: data
            mountPath: /media
            subPath: media
          {{- with $cfg.media_folder }}
          - name: folder
            mountPath: /data/{{ . }}
          {{- end }}
      volumes:
      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}
      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}
      {{- with $cfg.media_folder }}
      - name: folder
        persistentVolumeClaim:
          claimName: folder-{{ . }}
      {{- end }}
{{- end -}}
