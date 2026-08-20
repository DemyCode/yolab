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
Secret holding the platform account token, created per-namespace by local-api rather
than by the chart — a chart must not be able to choose where its credentials come from,
and keeping it out of values means it never lands in the Helm release Secret either.
*/}}
{{- define "yolab-common.tunnelSecretName" -}}
yolab-tunnel-credentials
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
    {{- /* The platform account token reaches ONLY this container, and only by
           reference. It used to be a literal env value, which put it in the pod spec
           — readable via `kubectl get deploy -o yaml`, present in the Helm release
           Secret because it was a chart value, and captured in every backup. That
           token is the whole account: it can read the raw B2 credentials from
           /storage/s3, manage tunnels and DNS, and it doubles as the x-yolab-cluster
           header that bypasses local-api auth entirely, including the root-shell
           endpoint. A single compromised app container should not be able to read it,
           and now the app's own containers never see it at all. */}}
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
{{- end -}}

{{/*
Same `/yolab/env` contract as the gateway pod, for an app that runs in its OWN pod.

Apps whose image listens on 80 or 443 cannot share the gateway pod — Caddy already
binds those there — so they get their own Deployment. But `/yolab/env` is an emptyDir
scoped to the gateway pod, which leaves a separate pod with no way to learn the
hostname the platform actually handed out. That matters for the apps that need to know
their own public URL: Nextcloud rejects a Host it does not trust, BookStack builds
every link from APP_URL, Ghost will not boot without one.

wg-register also persists its registration to the shared RWX PVC, so the FQDN is
readable from there. This init container waits for that file and writes exactly the
same `/yolab/env` the gateway pod's containers source, so an app container's startup
line is identical either way:

    . /yolab/env && export APP_URL="$YOLAB_URL" && exec …

It waits rather than races: on a first install this pod can easily start before the
platform has confirmed the subdomain, and reading a half-written or absent state file
would bake a blank URL into the app's config on its very first run — which for several
of these apps is the run that writes the permanent installation record.

Pairs with `yolab-common.yolabEnvVolumes`.
*/}}
{{- define "yolab-common.yolabEnvInit" -}}
- name: yolab-env
  {{- /* The wg-register image is reused purely because it already carries sh + jq. */}}
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

{{/*
Volumes for a pod using `yolabEnvInit`: the shared app PVC (where wg-register left its
state) and the emptyDir the resolved env lands in.
*/}}
{{- define "yolab-common.yolabEnvVolumes" -}}
- name: yolab
  emptyDir: {}
- name: data
  persistentVolumeClaim:
    claimName: {{ include "yolab-common.gateway.pvcName" . }}
{{- end -}}
