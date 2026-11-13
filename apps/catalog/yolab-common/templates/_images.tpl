

{{- define "yolab-common.image.wgRegister" -}}
{{- (((.Values.yolab).images).wgRegister) | default "ghcr.io/demycode/wg-register:main-latest@sha256:d32d0f30515ef94d3c89eb38a738d565a60f828953e920ba640d1eca73373bd9" -}}
{{- end -}}

{{- define "yolab-common.image.wgSidecar" -}}
{{- (((.Values.yolab).images).wgSidecar) | default "ghcr.io/demycode/wg-sidecar:main-latest@sha256:8b3b00ba5cf07e2f9827de5565709c53bf31f9d2dc603d31fe57ebc241e8c6fd" -}}
{{- end -}}

{{- define "yolab-common.image.caddy" -}}
{{- (((.Values.yolab).images).caddy) | default "ghcr.io/demycode/caddy:main-latest@sha256:6939186a01241a5a243371153dc4edf9d3fcc21c728980a10ee7ee72288c55fe" -}}
{{- end -}}
