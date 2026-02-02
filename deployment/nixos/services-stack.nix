{ config, pkgs, lib, modulesPath, ... }:
let
  configPath = ./ignored/config.toml;
  
  # Read and parse TOML configuration
  deployConfig = if builtins.pathExists configPath then
    builtins.fromTOML (builtins.readFile configPath)
  else
    throw ''
      Configuration file not found: ${toString configPath}
      
      Please create deployment/nixos/ignored/config.toml with your deployment settings.
      You can copy from deployment/nixos/ignored/config.toml.example:
      
        cp deployment/nixos/ignored/config.toml.example deployment/nixos/ignored/config.toml
      
      Then edit the file with your values.
    '';

  cfg = deployConfig;
in
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./disk-config.nix
    ./modules/services.nix
  ];

  boot.loader.grub = {
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  networking = {
    hostName = "yolab-services";
    enableIPv6 = true;
    useDHCP = true;
  };

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  users.users.root.openssh.authorizedKeys.keys = [
    cfg.ssh.public_key
  ];

  environment.systemPackages = with pkgs; [
    vim
    htop
    curl
    wget
    jq
  ];

  services.yolab-services = {
    enable = cfg.services.enable;
    repoUrl = cfg.server.repo_url;
    domain = cfg.server.domain;
    postgresDb = cfg.database.db_name;
    postgresUser = cfg.database.db_user;
    postgresPassword = cfg.database.db_password;
    ipv6SubnetBase = cfg.network.ipv6_subnet_base;
    frpsServerIpv6 = cfg.network.frps_server_ipv6;
  };

  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 30d";
  };

  system.stateVersion = "24.05";
}
