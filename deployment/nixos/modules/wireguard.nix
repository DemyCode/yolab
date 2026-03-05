{
  inputs,
  config,
  pkgs,
  lib,
  ...
}:

with lib;

let
  cfg = config.services.yolab-wireguard;

  workspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ../../../wireguard-manager;
  };
  overlay = workspace.mkPyprojectOverlay {
    sourcePreference = "wheel";
  };
  pythonSet =
    (pkgs.callPackage inputs.pyproject-nix.build.packages {
      python = pkgs.python311;
    }).overrideScope
      (
        lib.composeManyExtensions [
          inputs.pyproject-build-systems.overlays.wheel
          overlay
        ]
      );
  managerEnv = pythonSet.mkVirtualEnv "wireguard-manager-env" workspace.deps.default;
in
{
  options.services.yolab-wireguard = {
    enable = mkEnableOption "YoLab WireGuard Server";

    address = mkOption {
      type = types.str;
      description = "WireGuard interface IPv6 address with prefix length (e.g. 2a01:4f8::/64)";
    };

    listenPort = mkOption {
      type = types.int;
      default = 51820;
      description = "WireGuard UDP listen port";
    };

    privateKey = mkOption {
      type = types.str;
      description = "WireGuard server private key (base64)";
    };

    managerBackendUrl = mkOption {
      type = types.str;
      description = "Backend API URL for peer sync (host:port)";
    };

    managerPollInterval = mkOption {
      type = types.int;
      default = 30;
      description = "Peer sync interval in seconds";
    };
  };

  config = mkIf cfg.enable {
    environment.etc."wireguard/server.key" = {
      text = cfg.privateKey + "\n";
      mode = "0400";
    };

    networking.wireguard.interfaces.wg0 = {
      ips = [ cfg.address ];
      listenPort = cfg.listenPort;
      privateKeyFile = "/etc/wireguard/server.key";
      peers = [ ];
    };

    boot.kernel.sysctl = {
      "net.ipv4.ip_forward" = 1;
      "net.ipv6.conf.all.forwarding" = 1;
    };

    networking.firewall.allowedUDPPorts = [ cfg.listenPort ];

    systemd.services.wireguard-manager = {
      description = "YoLab WireGuard Peer Manager";
      after = [
        "network.target"
        "wireguard-wg0.service"
      ];
      wants = [ "wireguard-wg0.service" ];
      wantedBy = [ "multi-user.target" ];
      path = [ pkgs.wireguard-tools pkgs.iproute2 ];
      environment = {
        BACKEND_URL = cfg.managerBackendUrl;
        POLL_INTERVAL = toString cfg.managerPollInterval;
        WG_INTERFACE = "wg0";
        PYTHONUNBUFFERED = "1";
      };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "10s";
        ExecStart = "${managerEnv}/bin/wireguard-manager";
      };
    };

    environment.systemPackages = with pkgs; [
      wireguard-tools
    ] ++ [ managerEnv ];
  };
}
