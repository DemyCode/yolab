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
      default = "frp_services";
      description = "PostgreSQL database name";
    };

    postgresUser = mkOption {
      type = types.str;
      default = "frp_user";
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

    frpsServerIpv6 = mkOption {
      type = types.str;
      description = "FRP server IPv6 address";
    };

    frpsServerIpv4 = mkOption {
      type = types.str;
      description = "FRP server IPv4 address for FRPC clients to connect";
    };

    openFirewall = mkOption {
      type = types.bool;
      default = true;
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
        FRPS_SERVER_IPV6=${cfg.frpsServerIpv6}
        FRPS_SERVER_IPV4=${cfg.frpsServerIpv4}
        FRPS_SERVER_PORT=7000
        IPV6_SUBNET_BASE=${cfg.ipv6SubnetBase}
        EOF
                        
                ${pkgs.docker-compose}/bin/docker-compose up -d --build --remove-orphans
      '';
    };
  };
}
