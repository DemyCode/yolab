{{/*
Gateway building blocks, deliberately exposed as separate pieces rather than one
finished Deployment.

Roughly half the catalog runs its app container INSIDE the gateway pod (their
Caddyfile proxies to `localhost:PORT`) and the other half proxies to a separate
Service. A single canned Deployment can only serve one of those shapes, so each app
chart owns its own Deployment and composes the pieces it needs:

    spec:
      initContainers:
        {{- include "yolab-common.wgRegisterInit" . | nindent 8 }}
      containers:
        {{- include "yolab-common.gatewayContainers" . | nindent 8 }}
        - name: my-app
          ...
      volumes:
        {{- include "yolab-common.gatewayVolumes" . | nindent 8 }}

Values consumed (all under `.Values.yolab`):
  platformApiUrl  — passed to wg-register so it can register the tunnel
  accountToken    — platform bearer token
  serviceName     — the tunnel subdomain (the config field with `format: tunnel`)
  gateway.pvcName — PVC holding app data; the gateway stores its own state under a
                    subPath of it. Defaults to "<release>-data".
  gateway.upstream — what Caddy proxies to, e.g. "filebrowser:80" or "localhost:3000"
  gateway.caddyfile — full Caddyfile override when `upstream` isn't expressive enough
*/}}

{{/* PVC the gateway keeps its registration state in. */}}
{{- define "yolab-common.gateway.pvcName" -}}
{{- (((.Values.yolab).gateway).pvcName) | default (printf "%s-data" .Release.Name) -}}
{{- end -}}

{{/*
wg-register: claims the tunnel subdomain with the platform and drops a WireGuard
config + /yolab/env for the sidecar and Caddy to consume.
*/}}
{{- define "yolab-common.wgRegisterInit" -}}
- name: wg-register
  image: {{ include "yolab-common.image.wgRegister" . }}
  imagePullPolicy: IfNotPresent
  env:
    - name: PLATFORM_API_URL
      value: {{ ((.Values.yolab).platformApiUrl) | default "" | quote }}
    - name: ACCOUNT_TOKEN
      value: {{ ((.Values.yolab).accountToken) | default "" | quote }}
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

{{/*
The two containers that make an app reachable: the WireGuard tunnel and Caddy.

The WireGuard sidecar is the ONLY container in the catalog that legitimately needs
`privileged: true` (it creates a network interface). Keeping it here rather than in
each app chart is what makes "only the gateway may be privileged" a rule that can
actually be enforced against third-party charts later.
*/}}
{{- define "yolab-common.gatewayContainers" -}}
{{ include "yolab-common.wireguardContainer" . }}
{{ include "yolab-common.caddyContainer" . }}
{{- end -}}

{{/*
The tunnel itself, without an HTTP proxy in front. Game servers (minecraft, valheim)
expose raw TCP/UDP ports straight through WireGuard and have no Caddy at all, so they
compose this plus wgRegisterInit and nothing else.
*/}}
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
  livenessProbe:
    tcpSocket:
      port: 80
    initialDelaySeconds: 15
    periodSeconds: 20
    failureThreshold: 3
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

{{/*
Volumes the pieces above require. `data` is the app's own PVC — the gateway shares it
rather than claiming its own, keeping one volume per app.
*/}}
{{- define "yolab-common.gatewayVolumes" -}}
{{ include "yolab-common.tunnelVolumes" . }}
- name: caddy-config
  configMap:
    name: {{ printf "%s-caddy" .Release.Name }}
{{- end -}}

{{/*
Volumes needed by wg-register + the WireGuard sidecar alone — no Caddyfile ConfigMap,
since a Caddy-less app never creates one. Pairs with wireguardContainer.
*/}}
{{- define "yolab-common.tunnelVolumes" -}}
- name: wireguard
  emptyDir: {}
- name: yolab
  emptyDir: {}
- name: data
  persistentVolumeClaim:
    claimName: {{ include "yolab-common.gateway.pvcName" . }}
{{- end -}}

{{/*
Caddyfile ConfigMap. YOLAB_FQDN comes from /yolab/env, written by wg-register once
the tunnel subdomain is confirmed — so the served hostname is whatever the platform
actually handed out, not whatever the user typed.
*/}}
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
    {{- else }}
    {$YOLAB_FQDN} {
      reverse_proxy {{ required "yolab.gateway.upstream is required when no caddyfile is given" (((.Values.yolab).gateway).upstream) }}
    }
    {{- end }}
{{- end -}}
