

{{- define "yolab-common.image.wgRegister" -}}
{{- (((.Values.yolab).images).wgRegister) | default "ghcr.io/demycode/wg-register:main-latest@sha256:d32d0f30515ef94d3c89eb38a738d565a60f828953e920ba640d1eca73373bd9" -}}
{{- end -}}

{{- define "yolab-common.image.wgSidecar" -}}
{{- (((.Values.yolab).images).wgSidecar) | default "ghcr.io/demycode/wg-sidecar:main-latest@sha256:ec53881e1fb804129706a7dc1bbe129b2c9baed610bb527bc80673baeea37e25" -}}
{{- end -}}

{{- define "yolab-common.image.caddy" -}}
{{- (((.Values.yolab).images).caddy) | default "caddy:2@sha256:685ae3d7c34d9b5f8caeddc5d14051db7bbd88ff1f5a2fa0c16bd8ca328fb7af" -}}
{{- end -}}

{{- define "yolab-common.image.fileExplorer" -}}
{{- (((.Values.yolab).images).fileExplorer) | default "gtstef/filebrowser:2.1.2-beta@sha256:0ff80341ce8a9c1e3c5ac8ca319bdf9675092028b04ed97607bbfebc10810fff" -}}
{{- end -}}
