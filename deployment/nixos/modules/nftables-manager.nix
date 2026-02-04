{ config, pkgs, lib, ... }:

with lib;

let
  cfg = config.services.yolab-nftables-manager;
  repoRoot = ../../..;
  
  # Create a Python script with dependencies
  nftablesManagerScript = pkgs.writeScriptBin "nftables-manager" ''
    #!${pkgs.python3.withPackages (ps: with ps; [ requests ])}/bin/python3
    exec ${repoRoot}/nftables_manager/manager.py "$@"
  '';
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

    ipv6Subnet = mkOption {
      type = types.str;
      description = "IPv6 subnet to accept (e.g., 2a01:4f8:1c19:b96::/64)";
    };
  };

  config = mkIf cfg.enable {
    networking.nftables.enable = true;

    boot.kernel.sysctl = {
      "net.ipv6.conf.all.forwarding" = 1;
    };

    networking.localCommands = ''
      ${pkgs.iproute2}/bin/ip -6 route add local ${cfg.ipv6Subnet} dev lo || true
    '';

    systemd.services.nftables-manager = {
      description = "YoLab nftables Manager";
      after = [ "network.target" "yolab-deploy.service" ];
      wants = [ "yolab-deploy.service" ];
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
        ExecStart = "${pkgs.python3.withPackages (ps: with ps; [ requests ])}/bin/python3 ${repoRoot}/nftables_manager/manager.py";
      };
    };

    # Install required packages
    environment.systemPackages = with pkgs; [
      nftables
      iproute2
      python3
    ];
  };
}
