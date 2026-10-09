{{- define "yolab-common.image.tor" -}}
{{- (((.Values.yolab).images).tor) | default "docker.io/osminogin/tor-simple:0.4.9.14@sha256:ea335859425375b3294b15a5df61ca317ca4fdd143cf7a8d568bb2cd48217774" -}}
{{- end -}}

{{- define "yolab-common.image.tailscale" -}}
{{- (((.Values.yolab).images).tailscale) | default "docker.io/tailscale/tailscale:v1.102.5@sha256:c507f3a2a6ab1cabd8d809b98edeb41edbd5c3fb6ad9632ffd098b4c7d0b4065" -}}
{{- end -}}


{{- define "yolab-common.tor.enabled" -}}
{{- if (((.Values.config).tor_enabled)) -}}true{{- end -}}
{{- end -}}

{{- define "yolab-common.tailscale.enabled" -}}
{{- if (((.Values.config).tailscale_enabled)) -}}true{{- end -}}
{{- end -}}

{{- define "yolab-common.claimState" -}}
claim_state() {
  dir=$1
  owner=$(cat "$dir/.yolab-owner" 2>/dev/null || true)
  if [ -n "$owner" ] && [ "$owner" != "$POD_NAMESPACE" ]; then
    echo "$dir was copied from $owner, which may still be running with it; starting with a fresh identity"
    rm -rf "$dir"/* "$dir"/.[!.]*
  fi
  printf "%s\n" "$POD_NAMESPACE" > "$dir/.yolab-owner"
}
{{- end -}}

{{- define "yolab-common.podNamespaceEnv" -}}
- name: POD_NAMESPACE
  valueFrom:
    fieldRef:
      fieldPath: metadata.namespace
{{- end -}}

{{- define "yolab-common.tor.port" -}}18792{{- end -}}

{{- define "yolab-common.tailscale.port" -}}18791{{- end -}}

{{- define "yolab-common.tailscale.hostname" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- (get $cfg "tailscale_hostname") | default (get $cfg "subdomain") | default (((.Values.yolab).serviceName) | default .Release.Name) -}}
{{- end -}}


{{- define "yolab-common.privateAccessResources" -}}
{{- if eq (include "yolab-common.tor.enabled" .) "true" }}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ printf "%s-tor" .Release.Name }}
  namespace: {{ .Release.Namespace }}
data:
  torrc: |
    DataDirectory /var/lib/tor
    SocksPort 0
    Log notice stdout
    HiddenServiceDir /var/lib/tor/service
    HiddenServicePort 80 127.0.0.1:{{ include "yolab-common.tor.port" . }}
{{- end }}
{{- if eq (include "yolab-common.tailscale.enabled" .) "true" }}
---
apiVersion: v1
kind: Secret
metadata:
  name: {{ printf "%s-tailscale" .Release.Name }}
  namespace: {{ .Release.Namespace }}
type: Opaque
stringData:
  authkey: {{ required "tailscale_auth_key is required when Tailscale is on" (((.Values.config).tailscale_auth_key)) | quote }}
---
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ printf "%s-tailscale" .Release.Name }}
  namespace: {{ .Release.Namespace }}
data:
  serve.json: |
    {
      "TCP": {"443": {"HTTPS": true}},
      "Web": {
        "${TS_CERT_DOMAIN}:443": {
          "Handlers": {"/": {"Proxy": "http://127.0.0.1:{{ include "yolab-common.tailscale.port" . }}"}}
        }
      },
      "AllowFunnel": {"${TS_CERT_DOMAIN}:443": false}
    }
{{- end }}
{{- end -}}


{{- define "yolab-common.privateAccessInit" -}}
{{- if eq (include "yolab-common.tor.enabled" .) "true" }}
- name: tor-state
  image: {{ include "yolab-common.image.tor" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 0
    runAsGroup: 0
  env:
    {{- include "yolab-common.podNamespaceEnv" . | nindent 4 }}
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      {{- include "yolab-common.claimState" . | nindent 6 }}
      claim_state /var/lib/tor
      mkdir -p /var/lib/tor/service
      chown -R 100:101 /var/lib/tor
      chmod 700 /var/lib/tor /var/lib/tor/service
  volumeMounts:
    - name: data
      mountPath: /var/lib/tor
      subPath: {{ printf "%s/tor" .Release.Name | quote }}
{{- end }}
{{- end -}}


{{- define "yolab-common.privateAccessContainers" -}}
{{- if eq (include "yolab-common.tor.enabled" .) "true" }}
- name: tor
  image: {{ include "yolab-common.image.tor" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    runAsUser: 100
    runAsGroup: 101
  command:
    - /bin/sh
    - -c
    - |
      set -u
      tor -f /etc/yolab-tor/torrc &
      pid=$!
      while kill -0 "$pid" 2>/dev/null; do
        if [ -s /var/lib/tor/service/hostname ]; then
          echo "YOLAB_OUTPUT tor_url http://$(cat /var/lib/tor/service/hostname)/"
          sleep 600 & wait $!
        else
          sleep 2
        fi
      done
      wait "$pid"
  volumeMounts:
    - name: tor-config
      mountPath: /etc/yolab-tor
      readOnly: true
    - name: data
      mountPath: /var/lib/tor
      subPath: {{ printf "%s/tor" .Release.Name | quote }}
{{- end }}
{{- if eq (include "yolab-common.tailscale.enabled" .) "true" }}
- name: tailscale
  image: {{ include "yolab-common.image.tailscale" . }}
  imagePullPolicy: IfNotPresent
  env:
    - name: TS_AUTHKEY
      valueFrom:
        secretKeyRef:
          name: {{ printf "%s-tailscale" .Release.Name }}
          key: authkey
    - name: TS_AUTH_ONCE
      value: "true"
    - name: TS_HOSTNAME
      value: {{ include "yolab-common.tailscale.hostname" . | quote }}
    - name: TS_STATE_DIR
      value: /var/lib/tailscale
    - name: TS_USERSPACE
      value: "true"
    - name: TS_SOCKET
      value: /tmp/tailscaled.sock
    - name: TS_SERVE_CONFIG
      value: /etc/yolab-tailscale/serve.json
    - name: TS_KUBE_SECRET
      value: ""
    {{- include "yolab-common.podNamespaceEnv" . | nindent 4 }}
  command:
    - /bin/sh
    - -c
    - |
      set -u
      {{- include "yolab-common.claimState" . | nindent 6 }}
      claim_state /var/lib/tailscale
      /usr/local/bin/containerboot &
      pid=$!
      while kill -0 "$pid" 2>/dev/null; do
        name=$(tailscale --socket=/tmp/tailscaled.sock status --json --peers=false 2>/dev/null \
          | sed -n 's/.*"DNSName": *"\([^"]*\)\.".*/\1/p' | head -n 1)
        if [ -n "$name" ]; then
          echo "YOLAB_OUTPUT tailscale_url https://$name/"
          sleep 600 & wait $!
        else
          sleep 5
        fi
      done
      wait "$pid"
  volumeMounts:
    - name: tailscale-config
      mountPath: /etc/yolab-tailscale
      readOnly: true
    - name: data
      mountPath: /var/lib/tailscale
      subPath: {{ printf "%s/tailscale" .Release.Name | quote }}
{{- end }}
{{- end -}}


{{- define "yolab-common.privateAccessVolumes" -}}
{{- if eq (include "yolab-common.tor.enabled" .) "true" }}
- name: tor-config
  configMap:
    name: {{ printf "%s-tor" .Release.Name }}
{{- end }}
{{- if eq (include "yolab-common.tailscale.enabled" .) "true" }}
- name: tailscale-config
  configMap:
    name: {{ printf "%s-tailscale" .Release.Name }}
{{- end }}
{{- end -}}


{{- define "yolab-common.privateAccess.caddySites" -}}
{{- $upstream := (((.Values.yolab).gateway).upstream) -}}
{{- if and $upstream (eq (include "yolab-common.tailscale.enabled" .) "true") }}
http://:{{ include "yolab-common.tailscale.port" . }} {
  bind 127.0.0.1
  reverse_proxy {{ $upstream }} {
    header_up X-Forwarded-Proto https
  }
}
{{- end }}
{{- if and $upstream (eq (include "yolab-common.tor.enabled" .) "true") }}
http://:{{ include "yolab-common.tor.port" . }} {
  bind 127.0.0.1
  reverse_proxy {{ $upstream }}
}
{{- end }}
{{- end -}}
