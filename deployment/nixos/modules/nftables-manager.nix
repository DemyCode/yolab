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
      description = "HAProxy configuration file path";
    };
  };

  config = mkIf cfg.enable {
    networking.nftables.enable = true;

    boot.kernel.sysctl = {
      "net.ipv4.conf.all.route_localnet" = 1;
      "net.ipv4.ip_forward" = 1;
      "net.ipv6.conf.all.forwarding" = 1;
      "net.ipv4.conf.default.route_localnet" = 1; # Recommended for consistency
    };

    services.haproxy = {
      enable = true;
      config = ''
        # Main HAProxy config - managed by nftables-manager
        # Service-specific config is in ${cfg.nftables_file}
      '';
    };

    # Create a systemd override to include our generated config
    systemd.services.haproxy = {
      preStart = ''
        # Ensure the config directory exists with proper permissions
        mkdir -p $(dirname ${cfg.nftables_file})
        # Always create fresh config to ensure correct settings
        cat > ${cfg.nftables_file} <<'EOF'
global
    log /dev/log local0
    stats socket /run/haproxy/admin.sock mode 660 level admin
    stats timeout 30s

defaults
    log global
    mode tcp
    option tcplog
    timeout connect 5000ms
    timeout client 50000ms
    timeout server 50000ms
EOF
      '';
      
      serviceConfig = {
        # Override to use our dynamic config file
        ExecStart = lib.mkForce "${pkgs.haproxy}/sbin/haproxy -Ws -f ${cfg.nftables_file} -p /run/haproxy/haproxy.pid";
        ExecReload = lib.mkForce "${pkgs.bash}/bin/bash -c '${pkgs.haproxy}/sbin/haproxy -Ws -f ${cfg.nftables_file} -p /run/haproxy/haproxy.pid -sf $(cat /run/haproxy/haproxy.pid)'";
        # Ensure required directories exist
        StateDirectory = "haproxy";
        RuntimeDirectory = "haproxy";
      };
    };

    systemd.services.nftables-manager = {
      description = "YoLab HAProxy Config Manager";
      after = [
        "network.target"
        "haproxy.service"
      ];
      wants = [ "haproxy.service" ];
      wantedBy = [ "multi-user.target" ];
      path = [
        pkgs.haproxy
        pkgs.iproute2
      ];
      environment = {
        BACKEND_URL = cfg.backendUrl;
        POLL_INTERVAL = toString cfg.pollInterval;
        NFTABLES_FILE = cfg.nftables_file;
        LOG_LEVEL = "DEBUG";
Environment = "PYTHONUNBUFFERED=1"; 
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
        haproxy
        iproute2
        python3
      ])
      ++ [ installerNFtables ];
  };
}
