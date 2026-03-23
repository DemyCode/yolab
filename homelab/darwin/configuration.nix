{
  pkgs,
  lib,
  inputs,
  ...
}:
let
  configPath = ../ignored/config.toml;
  homelabConfig =
    if builtins.pathExists configPath then
      builtins.fromTOML (builtins.readFile configPath)
    else
      { };

  cfg = homelabConfig.homelab or { };
  hostname = cfg.hostname or "homelab-mac";
  timezone = cfg.timezone or "UTC";

  tunnelCfg = homelabConfig.tunnel or { };
  tunnelEnabled = tunnelCfg.enabled or false;
  wgSubnet = lib.optionalString tunnelEnabled (
    (lib.head (lib.splitString "::" tunnelCfg.sub_ipv6)) + "::/64"
  );

  clientUi = pkgs.buildNpmPackage {
    pname = "client-ui";
    version = "0.1.0";
    src = ../client-ui;
    npmDepsHash = "sha256-vB4y/Ct1i7An5uP6fTEUwEYhjZApT6ZpLMq3cs996NY=";
    installPhase = ''
      npm run build
      cp -r dist $out
    '';
  };

  localApiWorkspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ../local-api;
  };
  localApiOverlay = localApiWorkspace.mkPyprojectOverlay { sourcePreference = "wheel"; };
  localApiPythonSet =
    (pkgs.callPackage inputs.pyproject-nix.build.packages { python = pkgs.python311; }).overrideScope
      (lib.composeManyExtensions [
        inputs.pyproject-build-systems.overlays.wheel
        localApiOverlay
      ]);
  localApiEnv = localApiPythonSet.mkVirtualEnv "local-api-env" localApiWorkspace.deps.default;

  # Write wg0.conf from config.toml values
  wg0Conf = pkgs.writeText "wg0.conf" (lib.optionalString tunnelEnabled ''
    [Interface]
    PrivateKey = ${tunnelCfg.wg_private_key}
    Address = ${tunnelCfg.sub_ipv6}/128

    [Peer]
    PublicKey = ${tunnelCfg.wg_server_public_key}
    Endpoint = ${tunnelCfg.wg_server_endpoint}
    AllowedIPs = ${wgSubnet}
    PersistentKeepalive = 25
  '');
in
{
  networking.hostName = hostname;
  time.timeZone = timezone;

  # Nix settings
  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  nix.gc.automatic = true;
  services.nix-daemon.enable = true;

  environment.systemPackages = with pkgs; [
    curl
    git
    vim
    wget
    htop
    colima
    docker
    docker-compose
    wireguard-go
    wireguard-tools
  ];

  # nginx serves the client UI and proxies /api/ to local-api
  # Use a manual launchd daemon since nix-darwin's nginx module differs from NixOS
  environment.etc."yolab/nginx.conf".text = ''
    events {}
    http {
      include ${pkgs.nginx}/conf/mime.types;
      server {
        listen 0.0.0.0:80;
        ${lib.optionalString tunnelEnabled "listen [${tunnelCfg.sub_ipv6}]:80;"}
        root ${clientUi};
        location / {
          try_files $uri $uri/ /index.html;
        }
        location /api/ {
          proxy_pass http://127.0.0.1:3001;
          proxy_http_version 1.1;
          proxy_set_header Connection "";
          proxy_buffering off;
          proxy_cache off;
        }
      }
    }
  '';

  launchd.daemons.yolab-nginx = {
    serviceConfig = {
      ProgramArguments = [
        "${pkgs.nginx}/bin/nginx"
        "-c" "/etc/yolab/nginx.conf"
        "-g" "daemon off;"
      ];
      RunAtLoad = true;
      KeepAlive = true;
      StandardOutPath = "/var/log/yolab-nginx.log";
      StandardErrorPath = "/var/log/yolab-nginx-error.log";
    };
  };

  # local-api: handles UI update requests (git pull + darwin-rebuild)
  launchd.daemons.yolab-local-api = {
    serviceConfig = {
      ProgramArguments = [ "${localApiEnv}/bin/local-api" ];
      RunAtLoad = true;
      KeepAlive = true;
      StandardOutPath = "/var/log/yolab-local-api.log";
      StandardErrorPath = "/var/log/yolab-local-api-error.log";
      EnvironmentVariables = {
        YOLAB_REPO_PATH = "/opt/yolab";
        YOLAB_PLATFORM = "darwin";
        YOLAB_FLAKE_TARGET = "yolab-mac";
        # Ensure darwin-rebuild and nix are on PATH
        PATH = "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/bin:/bin";
      };
    };
  };

  # colima: provides Docker runtime via a lightweight Linux VM
  # Start once; colima registers its own launchd agent for persistence
  system.activationScripts.colima-start.text = ''
    if ! ${pkgs.colima}/bin/colima status 2>/dev/null | grep -q "running"; then
      ${pkgs.colima}/bin/colima start --runtime docker 2>/dev/null || true
    fi
  '';

  # WireGuard via wireguard-go (userspace) + wg-quick
  launchd.daemons.yolab-wireguard = lib.mkIf tunnelEnabled {
    serviceConfig = {
      ProgramArguments = [
        "/bin/sh" "-c"
        ''
          export WG_QUICK_USERSPACE_IMPLEMENTATION=${pkgs.wireguard-go}/bin/wireguard-go
          ${pkgs.wireguard-tools}/bin/wg-quick down ${wg0Conf} 2>/dev/null || true
          ${pkgs.wireguard-tools}/bin/wg-quick up ${wg0Conf}
        ''
      ];
      RunAtLoad = true;
      KeepAlive = false;
      StandardOutPath = "/var/log/yolab-wireguard.log";
      StandardErrorPath = "/var/log/yolab-wireguard-error.log";
    };
  };

  system.stateVersion = 5;
}
