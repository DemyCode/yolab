{
  config,
  pkgs,
  lib,
  modulesPath,
  ...
}:
let
  configPath = ./ignored/config-wireguard.json;

  deployConfig =
    if builtins.pathExists configPath then
      builtins.fromJSON (builtins.readFile configPath)
    else
      throw ''
        Configuration file not found: ${toString configPath}

        Please create deployment/nixos/ignored/config-wireguard.json with your deployment settings.
        You can copy from deployment/nixos/ignored/config-wireguard.json.example:

          cp deployment/nixos/ignored/config-wireguard.json.example deployment/nixos/ignored/config-wireguard.json
      '';

  cfg = deployConfig;
in
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./disk-config.nix
    ./modules/wireguard.nix
  ];

  boot.loader.grub = {
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  time.timeZone = "UTC";
  i18n.defaultLocale = "en_US.UTF-8";

  networking = {
    hostName = cfg.server.hostname;
    enableIPv6 = true;
    useDHCP = false;

    interfaces.enp1s0 = {
      useDHCP = true;

      ipv6.addresses = [
        {
          address = cfg.network.ipv6_address;
          prefixLength = 64;
        }
      ];
    };

    defaultGateway6 = {
      address = "fe80::1";
      interface = "enp1s0";
    };

    firewall = {
      enable = true;
      allowedTCPPorts = [ 22 ];
    };
  };

  services.openssh = {
    enable = true;
    settings = {
      PermitRootLogin = "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  users.users.root.openssh.authorizedKeys.keys = [ cfg.ssh.public_key ];

  services.yolab-wireguard = {
    enable = cfg.wireguard.enable;
    address = cfg.wireguard.address;
    listenPort = cfg.wireguard.listen_port;
    privateKey = cfg.wireguard.private_key;
    managerBackendUrl = cfg.network.backend_url;
    managerPollInterval = cfg.wireguard_manager.poll_interval;
  };

  environment.systemPackages = with pkgs; [
    vim
    htop
    curl
    git
    wireguard-tools
  ];

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  system.stateVersion = "24.05";
}
