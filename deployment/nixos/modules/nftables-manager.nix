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
  };

  config = mkIf cfg.enable {
    networking.nftables.enable = true;

    # Enable IPv6 forwarding
    boot.kernel.sysctl = {
      "net.ipv6.conf.all.forwarding" = 1;
    };

    systemd.services.nftables-manager = {
      description = "YoLab nftables Manager";
      after = [
        "network.target"
      ];
      wantedBy = [ "multi-user.target" ];

      environment = {
        BACKEND_URL = cfg.backendUrl;
        POLL_INTERVAL = toString cfg.pollInterval;
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
        nftables
        iproute2
        python3
      ])
      ++ [ installerNFtables ];
  };
}
