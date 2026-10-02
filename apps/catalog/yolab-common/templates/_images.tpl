

{{- define "yolab-common.image.wgRegister" -}}
{{- (((.Values.yolab).images).wgRegister) | default "ghcr.io/demycode/wg-register:main-latest@sha256:23cedfe6e8a7291a98d4e44207bf30b742a5314dbece3b45eed83caccec22236" -}}
{{- end -}}

{{- define "yolab-common.image.wgSidecar" -}}
{{- (((.Values.yolab).images).wgSidecar) | default "ghcr.io/demycode/wg-sidecar:latest@sha256:d7706338f231b0e54a8ac6c4a2940f5d9d8c2ac017a69dd378250359ee3d98c1" -}}
{{- end -}}

{{- define "yolab-common.image.caddy" -}}
{{- (((.Values.yolab).images).caddy) | default "caddy:2@sha256:ec18ee54aab3315c22e25f3b2babda73ff8007d39b13b3bd1bfffa2f0444c7d9" -}}
{{- end -}}

{{- define "yolab-common.image.fileExplorer" -}}
{{- (((.Values.yolab).images).fileExplorer) | default "gtstef/filebrowser:2.1.0-beta@sha256:329dfca21dcd04d1430f9f190a719d9e9ecb0fb9a916d26a56cd631876aebaf1" -}}
{{- end -}}
