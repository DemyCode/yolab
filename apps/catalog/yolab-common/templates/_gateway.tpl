


{{- define "yolab-common.gateway.pvcName" -}}
{{- (((.Values.yolab).gateway).pvcName) | default (printf "%s-data" .Release.Name) -}}
{{- end -}}


{{- define "yolab-common.tunnelSecretName" -}}
yolab-tunnel-credentials
{{- end -}}


{{- define "yolab-common.wgRegisterInit" -}}
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
  volumeMounts:
    - name: wireguard
      mountPath: /wireguard
    - name: yolab
      mountPath: /yolab
    - name: data
      mountPath: /state
      subPath: {{ printf "%s/yolab-state" .Release.Name | quote }}
{{- end -}}


{{- define "yolab-common.gatewayContainers" -}}
{{ include "yolab-common.wireguardContainer" . }}
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
  readinessProbe:
    tcpSocket:
      port: 80
    initialDelaySeconds: 5
    periodSeconds: 10
  volumeMounts:
    - name: data
      mountPath: /data
      subPath: {{ printf "%s/caddy" .Release.Name | quote }}
    - name: caddy-config
      mountPath: /etc/caddy/Caddyfile
      subPath: Caddyfile
    - name: yolab
      mountPath: /yolab
    {{- include "yolab-common.fileExplorer.volumeMounts" . | nindent 4 }}
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
    {{- else if eq (include "yolab-common.auth.enabled" .) "true" }}
    {$YOLAB_FQDN} {
      {{- include "yolab-common.fileExplorer.caddyHandle" . | nindent 6 }}
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
      {{- include "yolab-common.fileExplorer.caddyHandle" . | nindent 6 }}
      reverse_proxy {{ required "yolab.gateway.upstream is required when no caddyfile is given" (((.Values.yolab).gateway).upstream) }}
    }
    {{- end }}
{{- end -}}


{{- define "yolab-common.yolabEnvInit" -}}
- name: yolab-env
  
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
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
