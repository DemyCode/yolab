{{/*
Pinned images for the shared gateway.

Every reference is tag + digest. Kubernetes defaults `imagePullPolicy` to `Always`
for a `:latest` tag, which meant these were re-pulled on every pod restart and could
silently move underneath running apps. Charts may override any of them via
`.Values.yolab.images.*`, but the defaults are the contract.
*/}}

{{- define "yolab-common.image.wgRegister" -}}
{{- (((.Values.yolab).images).wgRegister) | default "ghcr.io/demycode/wg-register:main-latest@sha256:7c914fd218a480edc831867346a02eb5f9806c8cf7a76e6c0af8ae8de8bf9da7" -}}
{{- end -}}

{{- define "yolab-common.image.wgSidecar" -}}
{{- (((.Values.yolab).images).wgSidecar) | default "ghcr.io/demycode/wg-sidecar:latest@sha256:d7706338f231b0e54a8ac6c4a2940f5d9d8c2ac017a69dd378250359ee3d98c1" -}}
{{- end -}}

{{- define "yolab-common.image.caddy" -}}
{{- (((.Values.yolab).images).caddy) | default "caddy:2@sha256:ec18ee54aab3315c22e25f3b2babda73ff8007d39b13b3bd1bfffa2f0444c7d9" -}}
{{- end -}}
