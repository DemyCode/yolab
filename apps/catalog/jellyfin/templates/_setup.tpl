{{- define "jellyfin.setup" -}}
{{- $cfg := .Values.config -}}
{{- if $cfg.admin_password }}
- name: setup
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
  env:
    - name: PASSWORD
      value: {{ $cfg.admin_password | quote }}
    - name: LIBRARIES
      value: {{ $cfg.libraries | default list | toJson | quote }}
    - name: ROOT
      value: {{ with $cfg.media_folder }}{{ printf "/data/%s" . | quote }}{{ else }}"/media"{{ end }}
    - name: USERNAME_OUTPUT
      value: {{ include "yolab-stack.outputKey" (list . "username") | quote }}
    - name: PASSWORD_OUTPUT
      value: {{ include "yolab-stack.outputKey" (list . "password") | quote }}
    - name: API_KEY_OUTPUT
      value: {{ include "yolab-stack.outputKey" (list . "api_key") | quote }}
  command:
    - /bin/sh
    - -c
    - |
      set -u
      J=http://localhost:8096
      AUTH='MediaBrowser Client="YoLab", Device="YoLab", DeviceId="yolab-setup", Version="1"'
      TOKEN=""
      KEY=""
      json() {
        curl -fsS -H "Content-Type: application/json" "$@"
      }
      call() {
        json -H "Authorization: $AUTH, Token=\"$TOKEN\"" "$@"
      }
      wizard() {
        finished=$(curl -fsS "$J/System/Info/Public" | jq -r '.StartupWizardCompleted') || return 1
        [ "$finished" = true ] && return 0
        json -X POST "$J/Startup/Configuration" \
          -d '{"UICulture":"en-US","MetadataCountryCode":"US","PreferredMetadataLanguage":"en"}' >/dev/null || return 1
        curl -fsS "$J/Startup/User" >/dev/null || return 1
        json -X POST "$J/Startup/User" \
          -d "$(jq -n --arg p "$PASSWORD" '{Name: "admin", Password: $p}')" >/dev/null || return 1
        curl -fsS -X POST "$J/Startup/Complete" >/dev/null
      }
      login() {
        TOKEN=$(json -H "Authorization: $AUTH" -X POST "$J/Users/AuthenticateByName" \
          -d "$(jq -n --arg p "$PASSWORD" '{Username: "admin", Pw: $p}')" | jq -er '.AccessToken')
      }
      libraries() {
        have=$(call "$J/Library/VirtualFolders" | jq -r '.[].Name') || return 1
        printf '%s' "$LIBRARIES" | jq -c '.[]' | while read -r lib; do
          name=$(printf '%s' "$lib" | jq -r '.name')
          printf '%s\n' "$have" | grep -qxF "$name" && continue
          path="$ROOT/$(printf '%s' "$lib" | jq -r '.path')"
          if [ ! -d "$path" ]; then
            echo "library $name waits for $path to exist"
            exit 1
          fi
          query=$(printf '%s' "$lib" | jq -r --arg p "$path" \
            '"name=\(.name | @uri)&collectionType=\(.kind | @uri)&paths=\($p | @uri)&refreshLibrary=true"')
          call -X POST "$J/Library/VirtualFolders?$query" -d '{"LibraryOptions":{}}' >/dev/null || exit 1
        done
      }
      api_key() {
        KEY=$(call "$J/Auth/Keys" | jq -r 'first(.Items[] | select(.AppName == "YoLab") | .AccessToken) // empty') || return 1
        [ -n "$KEY" ] && return 0
        call -X POST "$J/Auth/Keys?app=YoLab" >/dev/null || return 1
        KEY=$(call "$J/Auth/Keys" | jq -er 'first(.Items[] | select(.AppName == "YoLab") | .AccessToken)')
      }
      until curl -fsS "$J/System/Info/Public" >/dev/null 2>&1; do sleep 5; done
      until wizard && login && api_key; do
        echo "jellyfin is up but not set up yet, retrying in 15s"
        sleep 15
      done
      echo "YOLAB_OUTPUT $USERNAME_OUTPUT admin"
      echo "YOLAB_OUTPUT $PASSWORD_OUTPUT $PASSWORD"
      echo "YOLAB_OUTPUT $API_KEY_OUTPUT $KEY"
      until libraries; do
        sleep 15
      done
      echo "jellyfin is set up"
      while :; do sleep 3600; done
  volumeMounts:
    - name: data
      mountPath: /media
      subPath: {{ .Release.Name }}/jellyfin/media
      readOnly: true
    {{- with $cfg.media_folder }}
    - name: folder
      mountPath: /data/{{ . }}
      readOnly: true
    {{- end }}
{{- end }}
{{- end -}}
