

{{- define "yolab-common.image.wgRegister" -}}
{{- (((.Values.yolab).images).wgRegister) | default "ghcr.io/demycode/wg-register:main-latest@sha256:d32d0f30515ef94d3c89eb38a738d565a60f828953e920ba640d1eca73373bd9" -}}
{{- end -}}

{{- define "yolab-common.image.wgSidecar" -}}
{{- (((.Values.yolab).images).wgSidecar) | default "ghcr.io/demycode/wg-sidecar:main-latest@sha256:ec53881e1fb804129706a7dc1bbe129b2c9baed610bb527bc80673baeea37e25" -}}
{{- end -}}

{{- define "yolab-common.image.caddy" -}}
{{- (((.Values.yolab).images).caddy) | default "caddy:2@sha256:f2a1290d0463aad60660d4ec134943f183ee2a5f6c3eb7bf32dd984f2f020772" -}}
{{- end -}}

{{- define "yolab-common.image.fileExplorer" -}}
{{- (((.Values.yolab).images).fileExplorer) | default "gtstef/filebrowser:2.1.0-beta@sha256:329dfca21dcd04d1430f9f190a719d9e9ecb0fb9a916d26a56cd631876aebaf1" -}}
{{- end -}}
