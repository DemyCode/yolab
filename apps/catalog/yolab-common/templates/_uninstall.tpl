{{/*
Tunnel cleanup, as a Helm pre-delete hook.

All 18 apps shipped a byte-identical `uninstall.yaml.j2` (verified by md5), which
local-api applied by hand and then waited on with
`kubectl wait job/uninstall --for=condition=complete --timeout=120s` before deleting
the namespace. Helm runs pre-delete hooks and waits for them as part of
`helm uninstall`, so that orchestration leaves local-api entirely — and unlike the
hand-rolled version, a hook that fails is surfaced rather than silently skipped.

Runs BEFORE the release's resources are removed, so the PVC holding the tunnel state
still exists when the Job reads it.

Usage, from any app chart:  {{ include "yolab-common.uninstallHook" . }}

A chart that needs teardown beyond tunnel deletion (e.g. calling an app's own admin
API, scrubbing a secret) sets `.Values.yolab.uninstallExtraCommand` in its own
values.yaml — a shell snippet appended after the tunnel cleanup, run in the same
container. That shares this container's image/tools (sh, curl, jq — see
yolab-common.image.wgRegister), which covers simple cases; a chart needing a
different runtime ships its own separate `helm.sh/hook: pre-delete` Job instead —
Helm runs every hook of a given type on the release, this one included, so nothing
here needs to change to support that.

activeDeadlineSeconds bounds the Job from its creation regardless of whether the pod
ever schedules. Without it, a pod that can never schedule (seen in practice: a second
overlapping uninstall whose PVC the first one already deleted) sits Pending forever —
backoffLimit never applies, since a scheduling failure isn't a container restart — and
`helm uninstall --wait` blocks until local-api's own outer timeout gives up on it.
*/}}
{{- define "yolab-common.uninstallHook" -}}
apiVersion: batch/v1
kind: Job
metadata:
  name: {{ .Release.Name }}-uninstall
  namespace: {{ .Release.Namespace }}
  annotations:
    "helm.sh/hook": pre-delete
    "helm.sh/hook-delete-policy": hook-succeeded,hook-failed
spec:
  ttlSecondsAfterFinished: 0
  activeDeadlineSeconds: 90
  backoffLimit: 1
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: cleanup
          image: {{ include "yolab-common.image.wgRegister" . }}
          imagePullPolicy: IfNotPresent
          env:
            {{- /* By reference, same as wg-register — see the note there. This hook is
                   the only other thing that legitimately needs the account token, since
                   deleting the tunnel is an account-scoped operation. */}}
            - name: ACCOUNT_TOKEN
              valueFrom:
                secretKeyRef:
                  name: {{ include "yolab-common.tunnelSecretName" . }}
                  key: account-token
            - name: PLATFORM_API_URL
              value: {{ ((.Values.yolab).platformApiUrl) | default "" | quote }}
          command: ["/bin/sh", "-c"]
          args:
            - |
              TUNNEL_ID=$(jq -r '.tunnel_id // empty' /state/wg-state.json 2>/dev/null || true)
              if [ -z "$TUNNEL_ID" ]; then
                echo "No tunnel state found, nothing to clean up"
              else
                echo "Deleting tunnel $TUNNEL_ID..."
                STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE \
                  -H "Authorization: Bearer $ACCOUNT_TOKEN" \
                  "$PLATFORM_API_URL/tunnels/$TUNNEL_ID")
                if [ "$STATUS" -ge 200 ] && [ "$STATUS" -lt 300 ]; then
                  echo "Tunnel $TUNNEL_ID deleted (HTTP $STATUS)"
                else
                  echo "Warning: DELETE /tunnels/$TUNNEL_ID returned HTTP $STATUS"
                fi
              fi
              {{- with .Values.yolab.uninstallExtraCommand }}
              echo "Running chart-declared uninstall cleanup..."
              {{ . }}
              {{- end }}
          volumeMounts:
            - name: data
              mountPath: /state
              subPath: {{ printf "%s/yolab-state" .Release.Name | quote }}
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: {{ include "yolab-common.gateway.pvcName" . }}
{{- end -}}
