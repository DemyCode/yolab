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
    hostName = "yolab-frps";
    enableIPv6 = true;
    useDHCP = true;
    firewall = {
      enable = true;
      allowedTCPPorts = [ 22 7000 ];  # SSH and FRPS
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

  users.users.frps = {
    isSystemUser = true;
    group = "frps";
    home = "/var/lib/frps";
    createHome = true;
  };

  users.groups.frps = {};

  # System packages
  environment.systemPackages = with pkgs; [
    frp
    vim
    htop
    curl
    git
  ];

  # FRPS service configuration
  systemd.services.frps = {
    description = "FRP Server";
    after = [ "network.target" ];
    wantedBy = [ "multi-user.target" ];
    
    serviceConfig = {
      Type = "simple";
      User = "frps";
      Group = "frps";
      Restart = "on-failure";
      RestartSec = "5s";
      ExecStart = "${pkgs.frp}/bin/frps -c /etc/frps/frps.ini";
    };
  };

  # FRPS configuration directory
  environment.etc."frps/frps.ini" = {
    text = ''
      [common]
      # Bind on all IPv6 addresses
      bind_addr = ::
      bind_port = 7000
      
      # Domain for subdomain resolution
      subdomain_host = REPLACE_DOMAIN
      
      # Logging
      log_file = /var/lib/frps/frps.log
      log_level = info
      max_pool_count = 10
      
      # Auth plugin for validation
      [plugin.user_auth]
      addr = REPLACE_AUTH_PLUGIN_ADDR
      path = /handler
      ops = NewProxy
    '';
    mode = "0644";
  };

  # Log directory with proper permissions
  systemd.tmpfiles.rules = [
    "d /var/lib/frps 0755 frps frps -"
  ];

  # Enable Nix flakes
  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  system.stateVersion = "24.05";
}
