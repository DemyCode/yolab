{{- define "yolab-common.apiKey.enabled" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- $off := and (hasKey $cfg "api_key_enabled") (not (get $cfg "api_key_enabled")) -}}
{{- if and (((.Values.yolab).apiKey).offered) (not $off) -}}true{{- end -}}
{{- end -}}


{{- define "yolab-common.apiKeyInit" -}}
{{- if eq (include "yolab-common.apiKey.enabled" .) "true" }}
- name: api-key-init
  image: {{ include "yolab-common.image.caddy" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      KEYFILE=/api-key/key
      if [ ! -s "$KEYFILE" ]; then
        tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 40 > "$KEYFILE"
        chmod 600 "$KEYFILE"
      fi
      KEY=$(cat "$KEYFILE")
      printf "export YOLAB_API_KEY='%s'\n" "$KEY" >> /yolab/env
      echo "YOLAB_OUTPUT api_key $KEY"
      . /yolab/env
      if [ -n "${YOLAB_FQDN:-}" ]; then
        echo "YOLAB_OUTPUT api_url https://$YOLAB_FQDN{{ ((.Values.yolab).apiKey).path | default "/" }}"
      fi
  volumeMounts:
    - name: yolab
      mountPath: /yolab
    - name: data
      mountPath: /api-key
      subPath: {{ printf "%s/api-key" .Release.Name | quote }}
{{- end }}
{{- end -}}


{{- define "yolab-common.apiKey.guard" -}}
{{- if eq (include "yolab-common.apiKey.enabled" .) "true" }}
@yolab_without_api_key not header Authorization "Bearer {$YOLAB_API_KEY}"
respond @yolab_without_api_key "This app needs its API key: send the header Authorization: Bearer <key>" 401
{{- end }}
{{- end -}}
