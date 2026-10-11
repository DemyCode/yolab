{{- define "jellyseerr.configure" -}}
{{- $cfg := .Values.config -}}
{{- $jellyfin := $cfg.media_server | default dict -}}
{{- $radarr := $cfg.radarr | default dict -}}
{{- $sonarr := $cfg.sonarr | default dict -}}
- name: configure
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
  env:
    - name: API_KEY_OUTPUT
      value: {{ include "yolab-stack.outputKey" (list . "api_key") | quote }}
    - name: JELLYFIN_URL
      value: {{ $jellyfin.internal_url | default "" | quote }}
    - name: JELLYFIN_USERNAME
      value: {{ $jellyfin.username | default "" | quote }}
    - name: JELLYFIN_PASSWORD
      value: {{ $jellyfin.password | default "" | quote }}
    - name: RADARR_URL
      value: {{ $radarr.internal_url | default "" | quote }}
    - name: RADARR_KEY
      value: {{ $radarr.api_key | default "" | quote }}
    - name: SONARR_URL
      value: {{ $sonarr.internal_url | default "" | quote }}
    - name: SONARR_KEY
      value: {{ $sonarr.api_key | default "" | quote }}
  command:
    - /bin/sh
    - -c
    - |
      set -u
      S=http://localhost:5055/api/v1
      until [ -s /app/config/settings.json ] && curl -fsS "$S/settings/public" >/dev/null 2>&1; do
        sleep 5
      done
      KEY=$(jq -r '.main.apiKey' /app/config/settings.json)
      echo "YOLAB_OUTPUT $API_KEY_OUTPUT $KEY"
      call() {
        curl -fsS -H "X-Api-Key: $KEY" -H "Content-Type: application/json" "$@"
      }
      address() {
        rest=${1#*://}
        rest=${rest%%/*}
        HOST=${rest%:*}
        PORT=${rest##*:}
        [ "$HOST" = "$rest" ] && PORT=80
        return 0
      }
      media_server() {
        [ -n "$JELLYFIN_URL" ] || return 0
        ready=$(curl -fsS "$S/settings/public" | jq -r '.initialized') || return 1
        [ "$ready" = true ] && return 0
        address "$JELLYFIN_URL"
        body=$(jq -n --arg u "$JELLYFIN_USERNAME" --arg p "$JELLYFIN_PASSWORD" --arg h "$HOST" \
          --argjson port "$PORT" '{username: $u, password: $p, hostname: $h, port: $port,
            useSsl: false, urlBase: "", email: "admin@yolab.local", serverType: 2}')
        curl -fsS -H "Content-Type: application/json" -X POST "$S/auth/jellyfin" -d "$body" >/dev/null || return 1
        libraries=$(call "$S/settings/jellyfin/library?sync=true" | jq -r '[.[].id] | join(",")') || return 1
        if [ -z "$libraries" ]; then
          echo "waiting for Jellyfin to have a library"
          return 1
        fi
        call "$S/settings/jellyfin/library?enable=$libraries" >/dev/null || return 1
        call -X POST "$S/settings/initialize" >/dev/null
      }
      arr() {
        [ -n "$2" ] || return 0
        address "$2"
        found=$(call -X POST "$S/settings/$1/test" -d "$(jq -n --arg h "$HOST" --argjson port "$PORT" \
          --arg k "$3" '{hostname: $h, port: $port, apiKey: $k, useSsl: false, baseUrl: ""}')") || return 1
        profile=$(printf '%s' "$found" | jq -c 'first(.profiles[]) // empty')
        folder=$(printf '%s' "$found" | jq -r 'first(.rootFolders[].path) // empty')
        if [ -z "$profile" ] || [ -z "$folder" ]; then
          echo "waiting for $4 to have a quality profile and a library"
          return 1
        fi
        body=$(jq -n --arg name "$4" --arg h "$HOST" --argjson port "$PORT" --arg k "$3" \
          --argjson profile "$profile" --arg dir "$folder" '{
            name: $name, hostname: $h, port: $port, apiKey: $k, useSsl: false, baseUrl: "",
            activeProfileId: $profile.id, activeProfileName: $profile.name, activeDirectory: $dir,
            is4k: false, isDefault: true, externalUrl: "", syncEnabled: false, preventSearch: false,
            tagRequests: false, tags: [], minimumAvailability: "released",
            enableSeasonFolders: true, seasonFolders: true, animeTags: []}')
        id=$(call "$S/settings/$1" | jq -r --arg n "$4" 'first(.[] | select(.name == $n) | .id) // empty') || return 1
        if [ -n "$id" ]; then
          call -X PUT "$S/settings/$1/$id" -d "$body" >/dev/null
        else
          call -X POST "$S/settings/$1" -d "$body" >/dev/null
        fi
      }
      until media_server \
        && arr radarr "$RADARR_URL" "$RADARR_KEY" "Radarr (YoLab)" \
        && arr sonarr "$SONARR_URL" "$SONARR_KEY" "Sonarr (YoLab)"; do
        echo "seerr is up but not wired yet, retrying in 15s"
        sleep 15
      done
      echo "seerr is wired"
      while :; do sleep 3600; done
  volumeMounts:
    - name: data
      mountPath: /app/config
      subPath: {{ .Release.Name }}/jellyseerr
      readOnly: true
{{- end -}}
