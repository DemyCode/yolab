{{- define "media-stack.arrKeys" -}}
- name: {{ .name }}
  image: python:3.15-alpine@sha256:2699b90f4d562a22e2dd076c82f964032f64814204bff36d271a54ba4cc2c7e2
  imagePullPolicy: IfNotPresent
  command:
    - python3
    - -c
    - |
      import os, re, secrets, sys
      APPS = ("sonarr", "radarr", "lidarr", "prowlarr")
      KEY = re.compile(r"<ApiKey>([A-Za-z0-9]{20,})</ApiKey>")

      def ensure(app):
          folder = "/arr/" + app
          path = folder + "/config.xml"
          if not os.path.isdir(folder):
              os.makedirs(folder)
              os.chown(folder, 1000, 1000)
          if not os.path.exists(path):
              tmp = "%s.yolab-%s" % (path, secrets.token_hex(4))
              with open(tmp, "w") as f:
                  f.write("<Config>\n  <ApiKey>%s</ApiKey>\n</Config>\n" % secrets.token_hex(16))
              os.chown(tmp, 1000, 1000)
              try:
                  os.link(tmp, path)
              except FileExistsError:
                  pass
              os.unlink(tmp)
          with open(path) as f:
              found = KEY.search(f.read())
          if found is None:
              print(
                  "%s has no API key in its config.xml: its apps need a sign-in until this pod restarts" % app,
                  file=sys.stderr,
              )
              return secrets.token_hex(32)
          return found.group(1)

      keys = {app: ensure(app) for app in APPS}
      {{- if .env }}
      with open("/yolab/env.d/keys", "w") as f:
          for app, key in keys.items():
              f.write("export %s_API_KEY='%s'\n" % (app.upper(), key))
      {{- end }}
  volumeMounts:
    - name: data
      mountPath: /arr
      subPath: config
    {{- if .env }}
    - name: yolab
      mountPath: /yolab
    {{- end }}
{{- end -}}
