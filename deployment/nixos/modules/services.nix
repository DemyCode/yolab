{
  config,
  pkgs,
  lib,
  ...
}:

with lib;

let
  cfg = config.services.yolab-services;
  repoRoot = ../../..;
in
{
  options.services.yolab-services = {
    enable = mkEnableOption "YoLab Services Stack";

    domain = mkOption {
      type = types.str;
      description = "Domain name";
    };

    postgresDb = mkOption {
      type = types.str;
      description = "PostgreSQL database name";
    };

    postgresUser = mkOption {
      type = types.str;
      description = "PostgreSQL user";
    };

    postgresPassword = mkOption {
      type = types.str;
      description = "PostgreSQL password";
    };

    ipv6SubnetBase = mkOption {
      type = types.str;
      description = "IPv6 subnet base for client allocation";
    };

    wgServerEndpoint = mkOption {
      type = types.str;
      description = "WireGuard server endpoint (ip:port) for client wg0.conf generation";
    };

    wgServerPublicKey = mkOption {
      type = types.str;
      description = "WireGuard server public key for client wg0.conf generation";
    };

    wgServerIpv6 = mkOption {
      type = types.str;
      description = "WireGuard server public IPv6 for DNS root domain resolution";
    };

    openFirewall = mkOption {
      type = types.bool;
      description = "Whether to open firewall ports";
    };
  };

  config = mkIf cfg.enable {
    virtualisation.docker.enable = true;

    users.users.yolab = {
      isNormalUser = true;
      extraGroups = [
        "wheel"
        "docker"
      ];
    };

    environment.systemPackages = with pkgs; [
      docker
      docker-compose
    ];

    systemd.services.yolab-deploy = {
      description = "YoLab Services Deployment";
      after = [
        "docker.service"
        "network-online.target"
      ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      path = [
        pkgs.docker
        pkgs.docker-compose
        pkgs.coreutils
        pkgs.rsync
      ];

      script = ''
                ${pkgs.rsync}/bin/rsync -a --delete ${repoRoot}/ /opt/yolab-services
                        
                cd /opt/yolab-services
                
                cat > .env << 'EOF'
        POSTGRES_DB=${cfg.postgresDb}
        POSTGRES_USER=${cfg.postgresUser}
        POSTGRES_PASSWORD=${cfg.postgresPassword}
        DOMAIN=${cfg.domain}
        WG_SERVER_ENDPOINT=${cfg.wgServerEndpoint}
        WG_SERVER_PUBLIC_KEY=${cfg.wgServerPublicKey}
        WG_SERVER_IPV6=${cfg.wgServerIpv6}
        IPV6_SUBNET_BASE=${cfg.ipv6SubnetBase}
        EOF
                        
                ${pkgs.docker-compose}/bin/docker-compose up -d --build --remove-orphans
      '';
    };
  };
}
