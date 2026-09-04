{{/*
A lightweight, read-only file browser for the app's own data. Every chart that
composes the shared gateway (caddyContainer + caddyConfigMap, see _gateway.tpl)
gets one automatically, on by default — no per-chart change needed. The install
form's toggle and the Outputs entries below are injected the same way, from
read_chart / chart_outputs_spec in apps.rs, for any chart that reaches this file;
a chart never declares any of this itself.

WHY NO NEW CONTAINER
---------------------
Caddy already runs in every gateway pod and already ships `file_server browse`
and `caddy hash-password`. Reusing it avoids a filebrowser sidecar — and the
per-instance user database that would need initialising, upgrading and chowning
(see the standalone filebrowser chart for what that costs) — multiplied across
the whole catalog.

WHY READ-ONLY
-------------
The ask is "get your data out and look at it without a terminal", not a second
way to edit an app's live database out from under it. `file_server browse` gives
exactly that: a directory listing with working downloads, no write path to
accidentally corrupt what the app itself is reading.

WHY ITS OWN PASSWORD, NOT AUTHELIA
-----------------------------------
`auth_enabled` (_authelia.tpl) is opt-in per app, and most installed apps don't
have it on. Riding on it would mean most apps ship their raw data browsable by
anyone with the tunnel URL, by default — not an acceptable default for something
that's on by default itself. So this always sits behind its own HTTP Basic Auth,
independent of whatever the app's own auth is doing.

WHY THE PASSWORD IS GENERATED HERE
------------------------------------
Caddy needs a bcrypt hash for `basic_auth`, and `caddy hash-password` is the
only thing on this pod that produces one — mirrors Authelia using its own binary
for its own hash format (_authelia.tpl). Doing it in the Caddy container's own
startup, rather than a separate init container, avoids a whole extra image just
to run one command. The plaintext is persisted on the app's own PVC — checked
before regenerating, same pattern as Authelia's secrets — so a pod restart or an
upgrade doesn't silently change the password on the user.
*/}}

{{/*
True unless `.Values.config.file_explorer_enabled` is explicitly `false`. Absent
means "not answered" (a fresh install form defaults the checkbox to on, but a
chart installed outside the UI, or an existing instance from before this field
existed, has no opinion) — and defaulting the empty case to enabled here, rather
than via Sprig's `default` (which cannot tell "absent" from "explicitly false"),
is what makes this actually default-on instead of impossible to turn off.
*/}}
{{- define "yolab-common.fileExplorer.enabled" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (hasKey $cfg "file_explorer_enabled") (eq (get $cfg "file_explorer_enabled") false) -}}
{{- else -}}true{{- end -}}
{{- end -}}

{{/*
Appended to the Caddy container's startup script, after `. /yolab/env` (so
YOLAB_FQDN is already sourced) and before `exec caddy run`. Requires the
`/browse-state` volumeMount below. Emits nothing at all when disabled.

The two `YOLAB_OUTPUT` lines follow the same convention wg-register already uses
for the app's own URL (see any chart's `yolab.io/outputs` annotation) — scanned
out of this container's log by the existing outputs mechanism (scan_outputs in
apps.rs), which is why chart_outputs_spec injects matching patterns for these
two keys wherever this file's enabled check would apply.
*/}}
{{- define "yolab-common.fileExplorer.startupScript" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
PWFILE=/browse-state/password
[ -f "$PWFILE" ] || tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 24 > "$PWFILE"
export FILE_EXPLORER_AUTH_HASH="$(caddy hash-password --plaintext "$(cat "$PWFILE")")"
echo "YOLAB_OUTPUT file_explorer_url https://${YOLAB_FQDN}/__yolab-files/"
echo "YOLAB_OUTPUT file_explorer_password $(cat "$PWFILE")"
{{- end -}}
{{- end -}}

{{/*
The route. Inserted first in the site block (see caddyConfigMap) so it wins
regardless of whether auth_enabled also matches `/` — Caddy's `handle` takes the
first match, same trick the Authelia portal already relies on for /authelia/*.
Emits nothing at all when disabled.
*/}}
{{- define "yolab-common.fileExplorer.caddyHandle" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
handle /__yolab-files/* {
  basic_auth {
    explorer {$FILE_EXPLORER_AUTH_HASH}
  }
  uri strip_prefix /__yolab-files
  root * /browse
  file_server browse
}
{{- end -}}
{{- end -}}

{{/*
The two extra volumeMounts the Caddy container needs, on top of what
caddyContainer already mounts: writable state for the generated password, and a
read-only view of the release's own tree on the shared PVC (the same one every
app's own data already lives on — see gatewayVolumes). Emits nothing at all when
disabled, so no extra subPath gets created on a PVC that never uses it.
*/}}
{{- define "yolab-common.fileExplorer.volumeMounts" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
- name: data
  mountPath: /browse-state
  subPath: {{ printf "%s/file-explorer" .Release.Name | quote }}
- name: data
  mountPath: /browse
  subPath: {{ .Release.Name | quote }}
  readOnly: true
{{- end -}}
{{- end -}}
