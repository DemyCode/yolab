{ config, pkgs, lib, modulesPath, ... }:
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./disk-config.nix
    ./modules/frps.nix
    ./modules/services.nix
  ];

  boot.loader.grub = {
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  networking = {
    hostName = "yolab-server";
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

  users.users.root.openssh.authorizedKeys.keys = [];

  environment.systemPackages = with pkgs; [
    vim
    htop
    curl
    wget
    jq
  ];

  services.yolab-frps = {
    enable = true;
    domain = "REPLACE_DOMAIN";
    authPluginAddr = "127.0.0.1:5000";
  };

  services.yolab-services = {
    enable = true;
    repoUrl = "REPLACE_REPO_URL";
    domain = "REPLACE_DOMAIN";
    postgresDb = "REPLACE_POSTGRES_DB";
    postgresUser = "REPLACE_POSTGRES_USER";
    postgresPassword = "REPLACE_POSTGRES_PASSWORD";
    ipv6SubnetBase = "REPLACE_IPV6_SUBNET_BASE";
    frpsServerIpv6 = "REPLACE_FRPS_SERVER_IPV6";
  };

  systemd.services.frps.after = [ "yolab-deploy.service" ];
  systemd.services.frps.wants = [ "yolab-deploy.service" ];

  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  
  nix.gc = {
    automatic = true;
    dates = "weekly";
    options = "--delete-older-than 30d";
  };

  system.stateVersion = "24.05";
}
