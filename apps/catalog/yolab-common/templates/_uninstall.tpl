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
  backoffLimit: 1
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: cleanup
          image: {{ include "yolab-common.image.wgRegister" . }}
          imagePullPolicy: IfNotPresent
          env:
            - name: ACCOUNT_TOKEN
              value: {{ ((.Values.yolab).accountToken) | default "" | quote }}
            - name: PLATFORM_API_URL
              value: {{ ((.Values.yolab).platformApiUrl) | default "" | quote }}
          command: ["/bin/sh", "-c"]
          args:
            - |
              TUNNEL_ID=$(jq -r '.tunnel_id // empty' /state/wg-state.json 2>/dev/null || true)
              if [ -z "$TUNNEL_ID" ]; then
                echo "No tunnel state found, nothing to clean up"
                exit 0
              fi
              echo "Deleting tunnel $TUNNEL_ID..."
              STATUS=$(curl -s -o /dev/null -w "%{http_code}" -X DELETE \
                -H "Authorization: Bearer $ACCOUNT_TOKEN" \
                "$PLATFORM_API_URL/tunnels/$TUNNEL_ID")
              if [ "$STATUS" -ge 200 ] && [ "$STATUS" -lt 300 ]; then
                echo "Tunnel $TUNNEL_ID deleted (HTTP $STATUS)"
              else
                echo "Warning: DELETE /tunnels/$TUNNEL_ID returned HTTP $STATUS"
              fi
          volumeMounts:
            - name: data
              mountPath: /state
              subPath: {{ printf "%s/yolab-state" .Release.Name | quote }}
      volumes:
        - name: data
          persistentVolumeClaim:
            claimName: {{ include "yolab-common.gateway.pvcName" . }}
{{- end -}}
