{{/*
A lightweight, read-only file browser for the app's own data — one route on the
SAME Caddy the app already runs, not a second Caddy process.

Structured like Authelia (_authelia.tpl) in every way that matters for visibility:
a chart that wants this declares `file_explorer_enabled` in its own
values.schema.json (default true), adds the two matching entries to its own
`yolab.io/outputs` annotation, and explicitly includes fileExplorerInit in its own
initContainers list, right after wgRegisterInit — nothing here is injected by
local-api. The one thing genuinely shared is the route itself, baked into
yolab-common.caddyConfigMap the same way Authelia's forward_auth block already is,
because that Caddyfile is itself already the one shared per-app resource every
chart composes rather than authors from scratch.

WHY ONE CADDY, NOT A SECOND CONTAINER
----------------------------------------
`caddy file-server` (the obvious "just serve some files" tool) has no auth flags
at all, so serving this safely still needs `basic_auth` — but that belongs in the
Caddyfile the app already has, as one more `handle` block, not a whole second
Caddy instance on its own port existing solely to hold one directive the first
one could just as well carry.

WHY READ-ONLY
-------------
The ask is "get your data out and look at it without a terminal", not a second way
to edit an app's live database out from under it. `file_server browse` gives
exactly that: a directory listing with working downloads, no write path to
accidentally corrupt what the app itself is reading.

WHY ITS OWN PASSWORD, NOT AUTHELIA
-----------------------------------
`auth_enabled` is opt-in per app, and most installed apps don't have it on. Riding
on it would mean most apps ship their raw data browsable by anyone with the tunnel
URL, by default — not an acceptable default for something that's on by default
itself. So this always sits behind its own HTTP Basic Auth, independent of
whatever the app's own auth is doing.

WHY THE PASSWORD IS GENERATED IN AN INIT CONTAINER
------------------------------------------------------
Caddy needs a bcrypt hash for `basic_auth`, and `caddy hash-password` is the only
thing on this image that produces one — mirrors Authelia using its own binary for
its own hash format. The plaintext is persisted on the app's own PVC — checked
before regenerating, same pattern as Authelia's secrets — so a pod restart or an
upgrade doesn't silently change the password on the user. The hash itself is
appended to /yolab/env as FILE_EXPLORER_AUTH_HASH — the same file wg-register
already writes and the gateway's Caddy already sources before it starts — so the
one Caddy container picks it up for free; running after wg-register (declaration
order) is what makes this an append rather than a clobber.
*/}}

{{/*
True unless `.Values.config.file_explorer_enabled` is explicitly `false`. Absent
means "not answered" (a fresh install form defaults the checkbox to on, but an
instance from before a chart added this field has no opinion) — and defaulting the
empty case to enabled here, rather than via Sprig's `default` (which cannot tell
"absent" from "explicitly false"), is what makes this actually default-on instead
of impossible to turn off.
*/}}
{{- define "yolab-common.fileExplorer.enabled" -}}
{{- $cfg := (.Values.config) | default dict -}}
{{- if and (hasKey $cfg "file_explorer_enabled") (eq (get $cfg "file_explorer_enabled") false) -}}
{{- else -}}true{{- end -}}
{{- end -}}

{{/*
Generates/persists the Basic Auth password and appends its bcrypt hash to
/yolab/env. Position this right after wgRegisterInit in the chart's own
initContainers list — it needs /yolab/env to already exist (to append rather than
clobber) and YOLAB_FQDN already sourced (for the YOLAB_OUTPUT url line below),
both written there by wg-register, same ordering Authelia's own init relies on.

The two `YOLAB_OUTPUT` lines follow the convention wg-register already uses for
the app's own URL: scanned out of this container's log by the existing outputs
mechanism (scan_outputs in apps.rs) — which is why a chart including this also
adds matching entries to its own `yolab.io/outputs` annotation (see filebrowser's
Chart.yaml for the existing `url` example this mirrors).
*/}}
{{- define "yolab-common.fileExplorerInit" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
- name: file-explorer-init
  image: {{ include "yolab-common.image.caddy" . }}
  imagePullPolicy: IfNotPresent
  command:
    - /bin/sh
    - -c
    - |
      set -eu
      . /yolab/env
      PWFILE=/browse-state/password
      [ -f "$PWFILE" ] || tr -dc 'A-Za-z0-9' < /dev/urandom | head -c 24 > "$PWFILE"
      HASH=$(caddy hash-password --plaintext "$(cat "$PWFILE")")
      {{- /* Single-quoted in the written line: unescaped, a bcrypt hash's own `$`
             characters (e.g. $2a$14$...) would be read back as shell variable
             expansions the next time this file is sourced, corrupting the value. */}}
      printf "export FILE_EXPLORER_AUTH_HASH='%s'\n" "$HASH" >> /yolab/env
      echo "YOLAB_OUTPUT file_explorer_url https://${YOLAB_FQDN}/__yolab-files/"
      echo "YOLAB_OUTPUT file_explorer_password $(cat "$PWFILE")"
  volumeMounts:
    - name: yolab
      mountPath: /yolab
    - name: data
      mountPath: /browse-state
      subPath: {{ printf "%s/file-explorer" .Release.Name | quote }}
{{- end -}}
{{- end -}}

{{/*
The extra volumeMount the gateway's own Caddy container needs to serve this — a
read-only view of the release's own tree on the shared PVC, the same one every
app's own data already lives on. Include from caddyContainer's volumeMounts (see
_gateway.tpl); emits nothing at all when disabled, so no extra subPath gets
touched on a PVC that never uses it.
*/}}
{{- define "yolab-common.fileExplorer.volumeMounts" -}}
{{- if eq (include "yolab-common.fileExplorer.enabled" .) "true" }}
- name: data
  mountPath: /browse
  subPath: {{ .Release.Name | quote }}
  readOnly: true
{{- end -}}
{{- end -}}

{{/*
The route itself. Included from yolab-common.caddyConfigMap, first in the site
block so it wins regardless of whether auth_enabled also matches `/` (Caddy's
`handle` takes the first match) — the same trick the Authelia portal already
relies on for /authelia/*. Emits nothing at all when disabled.
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
