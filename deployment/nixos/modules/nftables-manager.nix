{
  inputs,
  config,
  pkgs,
  lib,
  ...
}:

with lib;

let
  cfg = config.services.yolab-nftables-manager;
  workspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ../../../nftables-manager;
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
  installerNFtables = pythonSet.mkVirtualEnv "nftables-manager-env" workspace.deps.default;
in
{
  options.services.yolab-nftables-manager = {
    enable = mkEnableOption "YoLab nftables Manager";

    backendUrl = mkOption {
      type = types.str;
      description = "Backend API URL";
    };
    pollInterval = mkOption {
      type = types.int;
      description = "Polling interval in seconds";
    };
    nftables_file = mkOption {
      type = types.str;
      description = "nftables configuration options";
    };
  };

  config = mkIf cfg.enable {
    networking.nftables.enable = true;

    # Enable IPv6 forwarding and IPv4 NAT to localhost
    boot.kernel.sysctl = {
      "net.ipv4.conf.all.route_localnet" = 1;
      "net.ipv4.ip_forward" = 1;
      "net.ipv6.conf.all.forwarding" = 1;
    "net.ipv4.conf.default.route_localnet" = 1; # Recommended for consistency
    };

    systemd.services.nftables-manager = {
      description = "YoLab nftables Manager";
      after = [
        "network.target"
      ];
      wantedBy = [ "multi-user.target" ];
      path = [
        pkgs.socat
        pkgs.nftables
      ];
      environment = {
        BACKEND_URL = cfg.backendUrl;
        POLL_INTERVAL = toString cfg.pollInterval;
        NFTABLES_FILE = cfg.nftables_file;
        LOG_LEVEL = "DEBUG";
      };

      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "10s";
        ExecStart = "${installerNFtables}/bin/nftables-manager";
      };
    };

    environment.systemPackages =
      (with pkgs; [
        socat
        nftables
        iproute2
        python3
      ])
      ++ [ installerNFtables ];
  };
}
