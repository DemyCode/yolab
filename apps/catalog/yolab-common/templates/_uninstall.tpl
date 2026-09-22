
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
