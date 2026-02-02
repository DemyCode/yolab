{ config, pkgs, lib, ... }:

with lib;

let
  cfg = config.services.yolab-services;
in
{
  options.services.yolab-services = {
    enable = mkEnableOption "YoLab Services Stack";

    repoUrl = mkOption {
      type = types.str;
      description = "Git repository URL";
    };

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

    autoUpdate = mkOption {
      type = types.bool;
      default = true;
      description = "Enable automatic daily updates";
    };
  };

  config = mkIf cfg.enable {
    virtualisation.docker.enable = true;

    users.users.yolab = {
      isNormalUser = true;
      extraGroups = [ "wheel" "docker" ];
    };

    environment.systemPackages = with pkgs; [
      docker
      docker-compose
      git
    ];

    systemd.tmpfiles.rules = [
      "d /opt/yolab 0755 yolab yolab -"
      "d /opt/yolab/data 0755 yolab yolab -"
    ];

    systemd.services.yolab-deploy = {
      description = "YoLab Services Deployment";
      after = [ "docker.service" "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];
      
      path = [ pkgs.git pkgs.docker pkgs.docker-compose pkgs.coreutils ];
      
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        User = "root";
        WorkingDirectory = "/opt/yolab";
      };
      
      script = ''
        set -e
        
        if [ ! -d "/opt/yolab/repo/.git" ]; then
          ${pkgs.git}/bin/git clone ${cfg.repoUrl} /opt/yolab/repo
        else
          cd /opt/yolab/repo
          ${pkgs.git}/bin/git pull || true
        fi
        
        cd /opt/yolab/repo
        
        if [ ! -f .env ]; then
          cat > .env << 'EOF'
POSTGRES_DB=${cfg.postgresDb}
POSTGRES_USER=${cfg.postgresUser}
POSTGRES_PASSWORD=${cfg.postgresPassword}
DOMAIN=${cfg.domain}
FRPS_SERVER_IPV6=${cfg.frpsServerIpv6}
FRPS_SERVER_PORT=7000
IPV6_SUBNET_BASE=${cfg.ipv6SubnetBase}
EOF
        fi
        
        ${pkgs.docker-compose}/bin/docker-compose up -d --build --remove-orphans
      '';
    };

    systemd.services.yolab-update = mkIf cfg.autoUpdate {
      description = "Update YoLab Services";
      after = [ "yolab-deploy.service" ];
      
      path = [ pkgs.git pkgs.docker pkgs.docker-compose ];
      
      serviceConfig = {
        Type = "oneshot";
        User = "root";
        WorkingDirectory = "/opt/yolab/repo";
      };
      
      script = ''
        set -e
        cd /opt/yolab/repo
        ${pkgs.git}/bin/git pull
        ${pkgs.docker-compose}/bin/docker-compose pull
        ${pkgs.docker-compose}/bin/docker-compose up -d --build --remove-orphans
        ${pkgs.docker}/bin/docker system prune -f
      '';
    };

    systemd.timers.yolab-update = mkIf cfg.autoUpdate {
      description = "Update YoLab Services Timer";
      wantedBy = [ "timers.target" ];
      timerConfig = {
        OnCalendar = "daily";
        Persistent = true;
      };
    };
  };
}
