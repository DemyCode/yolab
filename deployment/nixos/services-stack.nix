{ config, pkgs, lib, modulesPath, ... }:
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./disk-config.nix
  ];

  # Boot configuration
  boot.loader.grub = {
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  # System settings
  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  # Networking
  networking = {
    hostName = "yolab-services";
    enableIPv6 = true;
    useDHCP = true;
    firewall = {
      enable = true;
      allowedTCPPorts = [ 22 80 443 5000 ];  # SSH, HTTP, HTTPS, API
      allowedUDPPorts = [ 53 ];  # DNS
    };
  };

  # SSH configuration
  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  # Users
  users.users.root.openssh.authorizedKeys.keys = [
    # Will be injected by Terraform
  ];

  users.users.yolab = {
    isNormalUser = true;
    extraGroups = [ "wheel" "docker" ];
    openssh.authorizedKeys.keys = [
      # Will be injected by Terraform
    ];
  };

  # Enable Docker
  virtualisation.docker.enable = true;

  # System packages
  environment.systemPackages = with pkgs; [
    docker
    docker-compose
    git
    vim
    htop
    curl
    wget
    jq
  ];

  # Setup directories
  systemd.tmpfiles.rules = [
    "d /opt/yolab 0755 yolab yolab -"
    "d /opt/yolab/data 0755 yolab yolab -"
  ];

  # Git clone and Docker Compose deployment service
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
      
      # Clone or update repository
      if [ ! -d "/opt/yolab/repo/.git" ]; then
        echo "Cloning YoLab repository..."
        ${pkgs.git}/bin/git clone REPLACE_REPO_URL /opt/yolab/repo
      else
        echo "Updating YoLab repository..."
        cd /opt/yolab/repo
        ${pkgs.git}/bin/git pull || true
      fi
      
      cd /opt/yolab/repo
      
      # Create .env file if it doesn't exist
      if [ ! -f .env ]; then
        echo "Creating .env file from secrets..."
        cat > .env << 'EOF'
POSTGRES_DB=REPLACE_POSTGRES_DB
POSTGRES_USER=REPLACE_POSTGRES_USER
POSTGRES_PASSWORD=REPLACE_POSTGRES_PASSWORD
DOMAIN=REPLACE_DOMAIN
FRPS_SERVER_IPV6=REPLACE_FRPS_SERVER_IPV6
FRPS_SERVER_PORT=7000
IPV6_SUBNET_BASE=REPLACE_IPV6_SUBNET_BASE
EOF
      fi
      
      # Start services
      echo "Starting Docker Compose services..."
      ${pkgs.docker-compose}/bin/docker-compose up -d --build --remove-orphans
      
      echo "Deployment complete!"
    '';
  };

  # Service to update and restart containers periodically (optional)
  systemd.services.yolab-update = {
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

  systemd.timers.yolab-update = {
    description = "Update YoLab Services Timer";
    wantedBy = [ "timers.target" ];
    timerConfig = {
      OnCalendar = "daily";
      Persistent = true;
    };
  };

  # Enable Nix flakes
  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  
  # Automatic garbage collection
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 30d";
  };

  system.stateVersion = "24.05";
}
