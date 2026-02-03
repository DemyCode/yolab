{ config, pkgs, lib, modulesPath, ... }:
let
  configPath = ./ignored/config-frps.json;
  
  # Read and parse JSON configuration
  deployConfig = if builtins.pathExists configPath then
    builtins.fromJSON (builtins.readFile configPath)
  else
    throw ''
      Configuration file not found: ${toString configPath}
      
      Please create deployment/nixos/ignored/config-frps.json with your deployment settings.
      You can copy from deployment/nixos/ignored/config-frps.json.example:
      
        cp deployment/nixos/ignored/config-frps.json.example deployment/nixos/ignored/config-frps.json
      
      Then edit the file with your values, or use Terraform to auto-generate it.
    '';

  cfg = deployConfig;
in
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./disk-config.nix
    ./modules/frps.nix
  ];

  boot.loader.grub = {
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  networking = {
    hostName = "yolab-frps";
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
    git
  ];

  services.yolab-frps = {
    enable = cfg.frps.enable;
    domain = cfg.server.domain;
    authPluginAddr = cfg.network.auth_plugin_addr;
  };

  nix.settings.experimental-features = [ "nix-command" "flakes" ];

  system.stateVersion = "24.05";
}
