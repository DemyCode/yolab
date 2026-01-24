{ config, pkgs, lib, ... }:

let
  configPath = ../config.toml;
  homelabConfig = if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else {};

  uiConfig = homelabConfig.client_ui or {};
  uiEnabled = uiConfig.enabled or true;
  uiPort = uiConfig.port or 8080;
  platformApiUrl = uiConfig.platform_api_url or "http://localhost:5000";

  pythonEnv = pkgs.python311.withPackages (ps: with ps; [
    fastapi
    uvicorn
    httpx
    toml
  ]);

  frontend = pkgs.buildNpmPackage {
    name = "yolab-client-ui-frontend";
    src = ../../client-ui/frontend;

    npmDepsHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";

    buildPhase = ''
      npm run build
    '';

    installPhase = ''
      mkdir -p $out
      cp -r dist/* $out/
    '';
  };

  clientUiApp = pkgs.stdenv.mkDerivation {
    name = "yolab-client-ui";
    src = ../../client-ui/backend;

    installPhase = ''
      mkdir -p $out/bin $out/lib/frontend/dist
      cp backend.py $out/lib/
      cp -r ${frontend}/* $out/lib/frontend/dist/
      cat > $out/bin/yolab-client-ui <<EOF
      #!${pkgs.bash}/bin/bash
      cd $out/lib
      exec ${pythonEnv}/bin/python3 backend.py
      EOF
      chmod +x $out/bin/yolab-client-ui
    '';
  };

in {
  config = lib.mkIf uiEnabled {
    systemd.tmpfiles.rules = [
      "d /etc/yolab 0755 root root -"
      "d /var/lib/yolab 0755 root root -"
      "d /var/lib/yolab/services 0755 root root -"
    ];

    systemd.services.yolab-client-ui = {
      description = "YoLab Client UI";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];

      environment = {
        PLATFORM_API_URL = platformApiUrl;
        CONFIG_PATH = "/etc/yolab/config.toml";
        SERVICES_DIR = "/var/lib/yolab/services";
      };

      serviceConfig = {
        Type = "simple";
        ExecStart = "${clientUiApp}/bin/yolab-client-ui";
        Restart = "always";
        RestartSec = "10s";
        WorkingDirectory = "${clientUiApp}/lib";
      };
    };
  };
}
