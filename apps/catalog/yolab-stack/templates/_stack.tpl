{{- define "yolab-stack.prefix" -}}
{{- with .Member }}{{ . }}-{{ end -}}
{{- end -}}


{{- define "yolab-stack.outputKey" -}}
{{- $ctx := index . 0 -}}
{{- with $ctx.Member }}{{ . }}_{{ end }}{{ index . 1 }}
{{- end -}}


{{- define "yolab-stack.memberRelease" -}}
{{- $root := index . 0 -}}
{{- printf "%s-%s" $root.Release.Name (index . 1) -}}
{{- end -}}


{{- define "yolab-stack.podLabels" -}}
app: {{ include "yolab-stack.prefix" . }}gateway
{{- with .Member }}
yolab.io/member: {{ . }}
{{- end }}
{{- end -}}


{{- define "yolab-stack.keep" -}}
{{- if .Member }}
annotations:
  helm.sh/resource-policy: keep
{{- end }}
{{- end -}}


{{- define "yolab-stack.member" -}}
{{- $root := index . 0 -}}
{{- $name := index . 1 -}}
{{- $config := index . 2 -}}
{{- $values := deepCopy (index $root.Values $name | default dict) -}}
{{- $cfg := deepCopy ($values.config | default dict) -}}
{{- range $key, $value := $config }}{{ if not (kindIs "invalid" $value) }}{{ $_ := set $cfg $key $value }}{{ end }}{{ end -}}
{{- $yolab := deepCopy ($values.yolab | default dict) -}}
{{- range $key, $value := ($root.Values.yolab | default dict) }}{{ $_ := set $yolab $key $value }}{{ end -}}
{{- $_ := set $yolab "serviceName" "" -}}
{{- with (($root.Values.yolab).serviceName) }}{{ $_ := set $yolab "serviceName" (printf "%s-%s" . $name) }}{{ end -}}
{{- $_ := set $values "config" $cfg -}}
{{- $_ := set $values "yolab" $yolab -}}
{{- $release := deepCopy $root.Release -}}
{{- $_ := set $release "Name" (include "yolab-stack.memberRelease" (list $root $name)) -}}
{{- $ctx := dict
      "Values" $values
      "Release" $release
      "Chart" (dict "Name" $name)
      "Capabilities" $root.Capabilities
      "Template" $root.Template
      "Files" $root.Files
      "Member" $name -}}
{{ include (printf "%s.manifests" $name) $ctx }}
{{- end -}}


{{- define "yolab-stack.secret" -}}
{{- $root := index . 0 -}}
{{- $name := index . 1 -}}
{{- $key := index . 2 -}}
{{- $existing := lookup "v1" "Secret" $root.Release.Namespace $name -}}
{{- if and $existing $existing.data (index $existing.data $key) -}}
{{- index $existing.data $key | b64dec -}}
{{- else -}}
{{- uuidv4 | replace "-" "" -}}
{{- end -}}
{{- end -}}


{{- define "yolab-stack.folderClaim" -}}
{{- $root := index . 0 -}}
{{- $folder := index . 1 -}}
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: folder-{{ $folder }}
  namespace: {{ $root.Release.Namespace }}
  labels:
    yolab.io/folder: {{ $folder | quote }}
spec:
  accessModes:
    - ReadWriteMany
  storageClassName: ""
  volumeName: {{ $root.Release.Namespace }}.folder-{{ $folder }}
  resources:
    requests:
      storage: 1Gi
{{- end -}}


{{- define "yolab-stack.ownFolderClaim" -}}
{{- $ctx := index . 0 -}}
{{- $folder := index . 1 -}}
{{- if and (not $ctx.Member) $folder }}
---
{{ include "yolab-stack.folderClaim" (list $ctx $folder) }}
{{- end }}
{{- end -}}


{{- define "yolab-stack.servarrKeyEnv" -}}
{{- $ctx := index . 0 -}}
{{- $app := index . 1 -}}
{{- with $ctx.Values.config.api_key }}
- name: {{ $app | upper }}__AUTH__APIKEY
  value: {{ . | quote }}
{{- end }}
{{- end -}}


{{- define "yolab-stack.servarrKeyScript" -}}
KEY="${API_KEY:-}"
if [ -z "$KEY" ]; then
  until [ -s /config/config.xml ]; do sleep 5; done
  KEY=$(sed -n 's:.*<ApiKey>\(.*\)</ApiKey>.*:\1:p' /config/config.xml | head -n 1)
fi
echo "YOLAB_OUTPUT $API_KEY_OUTPUT $KEY"
call() {
  curl -fsS -H "X-Api-Key: $KEY" -H "Content-Type: application/json" "$@"
}
upsert() {
  id=$(call "$API/$1" | jq -r --arg name "$2" 'first(.[] | select(.name == $name) | .id) // empty') || return 1
  if [ -n "$id" ]; then
    call -X PUT "$API/$1/$id?forceSave=true" -d "$(printf '%s' "$3" | jq --argjson id "$id" '. + {id: $id}')" >/dev/null
  else
    call -X POST "$API/$1?forceSave=true" -d "$3" >/dev/null
  fi
}
{{- end -}}


{{- define "yolab-stack.servarr" -}}
{{- $ctx := index . 0 -}}
{{- $p := index . 1 -}}
{{- $cfg := $ctx.Values.config -}}
{{- $client := $cfg.download_client | default dict -}}
- name: configure
  image: {{ include "yolab-common.image.wgRegister" $ctx }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 1000
    runAsGroup: 1000
  env:
    - name: API_KEY
      value: {{ $cfg.api_key | default "" | quote }}
    - name: API_KEY_OUTPUT
      value: {{ include "yolab-stack.outputKey" (list $ctx "api_key") | quote }}
    - name: API
      value: {{ printf "http://localhost:%v/api/%s" $p.port $p.api | quote }}
    - name: API_VERSION
      value: {{ $p.api | quote }}
    - name: ROOT_FOLDER
      value: {{ with $cfg.media_folder }}{{ printf "/data/%s/%s" . $p.library | quote }}{{ else }}""{{ end }}
    - name: CATEGORY_FIELD
      value: {{ $p.category | quote }}
    - name: CATEGORY
      value: {{ $p.app | quote }}
    - name: DC_URL
      value: {{ $client.internal_url | default "" | quote }}
    - name: DC_USERNAME
      value: {{ $client.username | default "" | quote }}
    - name: DC_PASSWORD
      value: {{ $client.password | default "" | quote }}
  command:
    - /bin/sh
    - -c
    - |
      set -u
      {{- include "yolab-stack.servarrKeyScript" $ctx | nindent 6 }}
      root_folder() {
        [ -n "$ROOT_FOLDER" ] || return 0
        mkdir -p "$ROOT_FOLDER" || return 1
        call "$API/rootfolder" | jq -e --arg p "$ROOT_FOLDER" \
          'any(.[]; (.path | rtrimstr("/")) == $p)' >/dev/null && return 0
        body=$(jq -n --arg p "$ROOT_FOLDER" '{path: $p, name: ($p | split("/") | last)}')
        if [ "$API_VERSION" = v1 ]; then
          quality=$(call "$API/qualityprofile" | jq -e 'first(.[].id)') || return 1
          metadata=$(call "$API/metadataprofile" | jq -e 'first(.[].id)') || return 1
          body=$(printf '%s' "$body" | jq --argjson q "$quality" --argjson m "$metadata" \
            '. + {defaultQualityProfileId: $q, defaultMetadataProfileId: $m,
                  defaultMonitorOption: "all", defaultNewItemMonitorOption: "all", defaultTags: []}')
        fi
        call -X POST "$API/rootfolder" -d "$body" >/dev/null
      }
      download_client() {
        [ -n "$DC_URL" ] || return 0
        rest=${DC_URL#*://}
        rest=${rest%%/*}
        host=${rest%:*}
        port=${rest##*:}
        [ "$host" = "$rest" ] && port=80
        template=$(call "$API/downloadclient/schema" \
          | jq -e 'first(.[] | select(.implementation == "QBittorrent"))') || return 1
        body=$(printf '%s' "$template" | jq --arg name "qBittorrent (YoLab)" --arg host "$host" \
          --argjson port "$port" --arg user "$DC_USERNAME" --arg pass "$DC_PASSWORD" \
          --arg field "$CATEGORY_FIELD" --arg category "$CATEGORY" '
            .name = $name | .enable = true | .priority = 1 | .tags = []
            | .fields |= map(
                if .name == "host" then .value = $host
                elif .name == "port" then .value = $port
                elif .name == "useSsl" then .value = false
                elif .name == "username" then .value = $user
                elif .name == "password" then .value = $pass
                elif .name == $field then .value = $category
                else . end)') || return 1
        upsert downloadclient "qBittorrent (YoLab)" "$body"
      }
      until call "$API/system/status" >/dev/null 2>&1; do sleep 5; done
      until root_folder && download_client; do
        echo "{{ $p.app }} is up but not wired yet, retrying in 15s"
        sleep 15
      done
      echo "{{ $p.app }} is wired${ROOT_FOLDER:+: its library is $ROOT_FOLDER}${DC_URL:+, downloads go to $DC_URL}"
      while :; do sleep 3600; done
  volumeMounts:
    - name: data
      mountPath: /config
      subPath: {{ $p.configSubPath | quote }}
      readOnly: true
    {{- with $cfg.media_folder }}
    - name: folder
      mountPath: /data/{{ . }}
    {{- end }}
{{- end -}}


{{- define "yolab-stack.urlOutput" -}}
{{- if .Member }}
{{- $url := include "yolab-stack.outputKey" (list . "url") }}
- name: stack-output
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      {{- if eq (include "yolab-common.yolab.enabled" .) "true" }}
      . /yolab/env
      if [ -n "${YOLAB_FQDN:-}" ]; then
        echo "YOLAB_OUTPUT {{ $url }} https://$YOLAB_FQDN"
      else
        echo "YOLAB_OUTPUT {{ $url }} NONE"
      fi
      {{- else }}
      echo "YOLAB_OUTPUT {{ $url }} NONE"
      {{- end }}
      {{- range ((.Values.config).stack_outputs_off) }}
      echo "YOLAB_OUTPUT {{ . }} NONE"
      {{- end }}
  volumeMounts:
    - name: yolab
      mountPath: /yolab
{{- end }}
{{- end -}}
