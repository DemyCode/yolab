


{{- define "yolab-common.gateway.pvcName" -}}
{{- (((.Values.yolab).gateway).pvcName) | default (printf "%s-data" .Release.Name) -}}
{{- end -}}


{{- define "yolab-common.tunnelSecretName" -}}
yolab-tunnel-credentials
{{- end -}}


{{- define "yolab-common.yolab.enabled" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (hasKey $cfg "yolab_enabled") (eq (get $cfg "yolab_enabled") false) -}}
{{- else -}}true{{- end -}}
{{- end -}}


{{- define "yolab-common.wgRegisterInit" -}}
{{- if ne (include "yolab-common.yolab.enabled" .) "true" }}
{{ include "yolab-common.yolabOffInit" . }}
{{- else }}
- name: wg-register
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  env:
    - name: PLATFORM_API_URL
      value: {{ ((.Values.yolab).platformApiUrl) | default "" | quote }}
    
    - name: ACCOUNT_TOKEN
      valueFrom:
        secretKeyRef:
          name: {{ include "yolab-common.tunnelSecretName" . }}
          key: account-token
    - name: SERVICE_NAME
      value: {{ ((.Values.yolab).serviceName) | default "" | quote }}
    {{- $aliases := list }}
    {{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
    {{- $aliases = append $aliases (printf "FILE_EXPLORER_FQDN=%s" (include "yolab-common.fileExplorer.subdomain" .)) }}
    {{- end }}
    {{- $base := ((.Values.yolab).serviceName) | default .Release.Name }}
    {{- $extra := (((.Values.yolab).gateway).aliases) | default dict }}
    {{- range $var := keys $extra | sortAlpha }}
    {{- $aliases = append $aliases (printf "%s=%s-%s" $var $base (get $extra $var)) }}
    {{- end }}
    {{- if $aliases }}
    - name: ALIASES
      value: {{ join " " $aliases | quote }}
    {{- end }}
    - name: POD_NAMESPACE
      valueFrom:
        fieldRef:
          fieldPath: metadata.namespace
  volumeMounts:
    - name: wireguard
      mountPath: /wireguard
    - name: yolab
      mountPath: /yolab
    - name: data
      mountPath: /state
      subPath: {{ printf "%s/yolab-state" .Release.Name | quote }}
{{- end }}
{{- end -}}


{{- define "yolab-common.yolabOffInit" -}}
- name: yolab-env
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  env:
    - name: ACCOUNT_TOKEN
      valueFrom:
        secretKeyRef:
          name: {{ include "yolab-common.tunnelSecretName" . }}
          key: account-token
          optional: true
    - name: PLATFORM_API_URL
      value: {{ ((.Values.yolab).platformApiUrl) | default "" | quote }}
  command:
    - /bin/sh
    - -c
    - |
      set -u
      TUNNEL_ID=$(jq -r '.tunnel_id // empty' /state/wg-state.json 2>/dev/null || true)
      if [ -n "$TUNNEL_ID" ]; then
        STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE \
          -H "Authorization: Bearer ${ACCOUNT_TOKEN:-}" \
          "$PLATFORM_API_URL/tunnels/$TUNNEL_ID" || echo 000)
        case "$STATUS" in
          2??|404)
            rm -f /state/wg-state.json
            echo "YoLab address switched off: tunnel $TUNNEL_ID removed" ;;
          *)
            echo "YoLab address switched off, but removing tunnel $TUNNEL_ID returned HTTP $STATUS; will retry on the next start" ;;
        esac
      fi
      printf 'export YOLAB_FQDN=\nexport YOLAB_URL=\n' > /yolab/env
  volumeMounts:
    - name: yolab
      mountPath: /yolab
    - name: data
      mountPath: /state
      subPath: {{ printf "%s/yolab-state" .Release.Name | quote }}
{{- end -}}


{{- define "yolab-common.gatewayContainers" -}}
{{- if eq (include "yolab-common.yolab.enabled" .) "true" }}
{{ include "yolab-common.wireguardContainer" . }}
{{- end }}
{{ include "yolab-common.caddyContainer" . }}
{{- end -}}


{{- define "yolab-common.wireguardContainer" -}}
- name: wireguard
  image: {{ include "yolab-common.image.wgSidecar" . }}
  imagePullPolicy: IfNotPresent
  securityContext:
    privileged: true
  volumeMounts:
    - name: wireguard
      mountPath: /etc/wireguard
{{- end -}}

{{- define "yolab-common.caddyContainer" -}}
- name: caddy
  image: {{ include "yolab-common.image.caddy" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      . /yolab/env && exec caddy run --config /etc/caddy/Caddyfile --adapter caddyfile
  ports:
    - containerPort: 80
    - containerPort: 443
  {{- if eq (include "yolab-common.yolab.enabled" .) "true" }}
  readinessProbe:
    tcpSocket:
      port: 80
    initialDelaySeconds: 5
    periodSeconds: 10
  {{- end }}
  volumeMounts:
    - name: data
      mountPath: /data
      subPath: {{ printf "%s/caddy" .Release.Name | quote }}
    - name: caddy-config
      mountPath: /etc/caddy/Caddyfile
      subPath: Caddyfile
    - name: yolab
      mountPath: /yolab
{{- end -}}


{{- define "yolab-common.gatewayVolumes" -}}
{{ include "yolab-common.tunnelVolumes" . }}
- name: caddy-config
  configMap:
    name: {{ printf "%s-caddy" .Release.Name }}
{{- end -}}


{{- define "yolab-common.tunnelVolumes" -}}
- name: wireguard
  emptyDir: {}
- name: yolab
  emptyDir: {}
- name: data
  persistentVolumeClaim:
    claimName: {{ include "yolab-common.gateway.pvcName" . }}
{{- end -}}


{{- define "yolab-common.caddyConfigMap" -}}
apiVersion: v1
kind: ConfigMap
metadata:
  name: {{ printf "%s-caddy" .Release.Name }}
  namespace: {{ .Release.Namespace }}
data:
  Caddyfile: |
    {{- if (((.Values.yolab).gateway).caddyfile) }}
    {{- .Values.yolab.gateway.caddyfile | nindent 4 }}
    {{- else if ne (include "yolab-common.yolab.enabled" .) "true" }}
    {{- else if eq (include "yolab-common.auth.enabled" .) "true" }}
    {$YOLAB_FQDN} {
      # The portal, on this app's own domain. Must be matched BEFORE the
      # forward_auth below, or the login page would itself require a login.
      handle /authelia/* {
        reverse_proxy localhost:9091
      }
      handle {
        forward_auth localhost:9091 {
          uri /authelia/api/authz/forward-auth
          # Passed to the app so it can know who is signed in. Harmless for an
          # app that ignores them, and the only way one can personalise.
          copy_headers Remote-User Remote-Groups Remote-Email Remote-Name
        }
        reverse_proxy {{ required "yolab.gateway.upstream is required when no caddyfile is given" (((.Values.yolab).gateway).upstream) }}
      }
    }
    {{- else }}
    {$YOLAB_FQDN} {
      reverse_proxy {{ required "yolab.gateway.upstream is required when no caddyfile is given" (((.Values.yolab).gateway).upstream) }}
    }
    {{- end }}
    {{- include "yolab-common.fileExplorer.caddySite" . | nindent 4 }}
    {{- include "yolab-common.privateAccess.caddySites" . | nindent 4 }}
{{- end -}}


{{- define "yolab-common.yolabEnvInit" -}}
- name: yolab-env
  
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      {{- if ne (include "yolab-common.yolab.enabled" .) "true" }}
      printf 'export YOLAB_FQDN=\nexport YOLAB_URL=\n' > /yolab/env
      exit 0
      {{- end }}
      until [ -s /state/wg-state.json ]; do
        echo "waiting for the tunnel to be registered..."
        sleep 2
      done
      FQDN=$(jq -r '.fqdn // empty' /state/wg-state.json)
      if [ -z "$FQDN" ]; then
        echo "wg-state.json carries no fqdn — refusing to start with a blank URL" >&2
        exit 1
      fi
      printf 'export YOLAB_FQDN=%s\nexport YOLAB_URL=https://%s\n' "$FQDN" "$FQDN" > /yolab/env
      echo "resolved YOLAB_FQDN=$FQDN"
  volumeMounts:
    - name: data
      mountPath: /state
      subPath: {{ printf "%s/yolab-state" .Release.Name | quote }}
    - name: yolab
      mountPath: /yolab
{{- end -}}


{{- define "yolab-common.yolabEnvVolumes" -}}
- name: yolab
  emptyDir: {}
- name: data
  persistentVolumeClaim:
    claimName: {{ include "yolab-common.gateway.pvcName" . }}
{{- end -}}
