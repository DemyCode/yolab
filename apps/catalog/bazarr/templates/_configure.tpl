{{- define "bazarr.configure" -}}
{{- $cfg := .Values.config -}}
{{- $sonarr := $cfg.sonarr | default dict -}}
{{- $radarr := $cfg.radarr | default dict -}}
- name: configure
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
  env:
    - name: API_KEY_OUTPUT
      value: {{ include "yolab-stack.outputKey" (list . "api_key") | quote }}
    - name: LANGUAGES
      value: {{ $cfg.subtitle_languages | default (list "en") | join " " | quote }}
    - name: SONARR_URL
      value: {{ $sonarr.internal_url | default "" | quote }}
    - name: SONARR_KEY
      value: {{ $sonarr.api_key | default "" | quote }}
    - name: RADARR_URL
      value: {{ $radarr.internal_url | default "" | quote }}
    - name: RADARR_KEY
      value: {{ $radarr.api_key | default "" | quote }}
  command:
    - /bin/sh
    - -c
    - |
      set -u
      B=http://localhost:6767/api
      CONFIG=/config/config/config.yaml
      until [ -s "$CONFIG" ] && curl -fsS -o /dev/null "http://localhost:6767/"; do
        sleep 5
      done
      KEY=$(awk '/^auth:/ { inside = 1; next } /^[^ ]/ { inside = 0 }
        inside && $1 == "apikey:" { print $2; exit }' "$CONFIG" | tr -d "'\"")
      echo "YOLAB_OUTPUT $API_KEY_OUTPUT $KEY"
      settings() {
        curl -fsS -H "X-API-KEY: $KEY" -X POST "$B/system/settings" "$@" >/dev/null
      }
      address() {
        rest=${1#*://}
        rest=${rest%%/*}
        HOST=${rest%:*}
        PORT=${rest##*:}
        [ "$HOST" = "$rest" ] && PORT=80
        return 0
      }
      arr() {
        [ -n "$2" ] || return 0
        address "$2"
        settings \
          --data-urlencode "settings-general-use_$1=true" \
          --data-urlencode "settings-$1-ip=$HOST" \
          --data-urlencode "settings-$1-port=$PORT" \
          --data-urlencode "settings-$1-apikey=$3" \
          --data-urlencode "settings-$1-base_url=" \
          --data-urlencode "settings-$1-ssl=false"
      }
      languages() {
        have=$(curl -fsS -H "X-API-KEY: $KEY" "$B/system/languages/profiles" | jq 'length') || return 1
        [ "$have" -gt 0 ] && return 0
        profile=$(printf '%s\n' $LANGUAGES | jq -R . | jq -sc '[{
          profileId: 1, name: "YoLab", cutoff: null, mustContain: [], mustNotContain: [],
          originalFormat: false,
          items: (to_entries | map({id: (.key + 1), language: .value,
            audio_exclude: "False", hi: "False", forced: "False"}))}]')
        set --
        for code in $LANGUAGES; do
          set -- "$@" --data-urlencode "languages-enabled=$code"
        done
        settings "$@" \
          --data-urlencode "languages-profiles=$profile" \
          --data-urlencode "settings-general-serie_default_enabled=true" \
          --data-urlencode "settings-general-serie_default_profile=1" \
          --data-urlencode "settings-general-movie_default_enabled=true" \
          --data-urlencode "settings-general-movie_default_profile=1" \
          --data-urlencode "settings-general-enabled_providers=podnapisi" \
          --data-urlencode "settings-general-enabled_providers=yifysubtitles"
      }
      until arr sonarr "$SONARR_URL" "$SONARR_KEY" \
        && arr radarr "$RADARR_URL" "$RADARR_KEY" \
        && languages; do
        echo "bazarr is up but not wired yet, retrying in 15s"
        sleep 15
      done
      echo "bazarr is wired"
      while :; do sleep 3600; done
  volumeMounts:
    - name: data
      mountPath: /config
      subPath: '{{ .Release.Name }}/data/config'
      readOnly: true
{{- end -}}
