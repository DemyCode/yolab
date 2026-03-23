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
  hostname = cfg.hostname or "homelab-wsl";
  timezone = cfg.timezone or "UTC";
  locale = cfg.locale or "en_US.UTF-8";
  allowedSshKeys = cfg.allowed_ssh_keys or [ ];
  rootSshKey = cfg.root_ssh_key or "";
  homelabPasswordHash = cfg.homelab_password_hash or "";

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
in
{
  wsl = {
    enable = true;
    defaultUser = "homelab";
  };

  time.timeZone = timezone;
  i18n.defaultLocale = locale;

  networking = {
    hostName = hostname;
    enableIPv6 = true;
    firewall.enable = false;

    wireguard.interfaces = lib.mkIf tunnelEnabled {
      wg0 = {
        ips = [ "${tunnelCfg.sub_ipv6}/128" ];
        privateKey = tunnelCfg.wg_private_key;
        peers = [
          {
            publicKey = tunnelCfg.wg_server_public_key;
            endpoint = tunnelCfg.wg_server_endpoint;
            allowedIPs = [ wgSubnet ];
            persistentKeepalive = 25;
          }
        ];
      };
    };
  };

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = lib.mkIf (rootSshKey != "") "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  services.nginx = {
    enable = true;
    virtualHosts."default" = {
      default = true;
      listen = [
        { addr = "0.0.0.0"; port = 80; }
        { addr = "[::]"; port = 80; }
      ]
      ++ lib.optionals tunnelEnabled [
        { addr = "[${tunnelCfg.sub_ipv6}]"; port = 80; }
      ];
      root = "${clientUi}";
      locations."/" = {
        tryFiles = "$uri $uri/ /index.html";
      };
      locations."/api/" = {
        proxyPass = "http://127.0.0.1:3001";
        extraConfig = ''
          proxy_http_version 1.1;
          proxy_set_header Connection "";
          proxy_buffering off;
          proxy_cache off;
        '';
      };
    };
  };

  systemd.services.yolab-local-api = {
    description = "YoLab Local API";
    after = [ "network.target" ];
    wantedBy = [ "multi-user.target" ];
    path = [ pkgs.git pkgs.nix ];
    environment = {
      YOLAB_REPO_PATH = "/etc/nixos";
      YOLAB_PLATFORM = "wsl";
      YOLAB_FLAKE_TARGET = "yolab-wsl";
    };
    serviceConfig = {
      Type = "simple";
      User = "root";
      Restart = "always";
      RestartSec = "5s";
      ExecStart = "${localApiEnv}/bin/local-api";
    };
  };

  users.users.root.openssh.authorizedKeys.keys = lib.optional (rootSshKey != "") rootSshKey;

  users.users.homelab = {
    isNormalUser = true;
    extraGroups = [ "wheel" "docker" ];
    openssh.authorizedKeys.keys = allowedSshKeys;
    hashedPassword = lib.mkIf (homelabPasswordHash != "") homelabPasswordHash;
  };

  virtualisation.docker.enable = true;

  environment.systemPackages =
    with pkgs;
    map lib.lowPrio [
      curl
      gitMinimal
      just
      wireguard-tools
      docker
      docker-compose
      vim
      wget
      htop
    ];

  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  nix.gc.automatic = true;
  system.stateVersion = "24.05";
}
