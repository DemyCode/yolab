{
  pkgs,
  lib,
  ...
}: let
  # Read config.toml from ignored/ directory (relative path)
  configPath = ../../ignored/config.toml;
  homelabConfig =
    if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else {};

  uiConfig = homelabConfig.client_ui or {};
  uiEnabled = uiConfig.enabled or true;
  uiPort = uiConfig.port or 8080;
  platformApiUrl = uiConfig.platform_api_url or "http://localhost:5000";

  pythonEnv = pkgs.python311.withPackages (ps:
    with ps; [
      fastapi
      uvicorn
      httpx
    ]);

  # Build frontend (if it exists)
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

  # Package the client-ui backend
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
    # Systemd service for client-ui
    systemd.services.yolab-client-ui = {
      description = "YoLab Client UI";
      after = ["network.target"];
      wantedBy = ["multi-user.target"];

      environment = {
        PLATFORM_API_URL = platformApiUrl;
        # Config is at /etc/yolab/config.toml (copied by configuration.nix)
        CONFIG_PATH = "/etc/yolab/config.toml";
        SERVICES_DIR = "/var/lib/yolab/services";
        PORT = toString uiPort;
        # Flake path for nixos-rebuild (default assumes /opt/yolab)
        FLAKE_PATH = "/opt/yolab/homelab#yolab";
      };

      serviceConfig = {
        Type = "simple";
        ExecStart = "${clientUiApp}/bin/yolab-client-ui";
        Restart = "always";
        RestartSec = "10s";
        WorkingDirectory = "${clientUiApp}/lib";
      };
    };

    # Open firewall port for client-ui
    networking.firewall.allowedTCPPorts = [uiPort];
  };
}
