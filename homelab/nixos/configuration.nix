{
  modulesPath,
  pkgs,
  lib,
  ...
}:
let
  configPath = ../ignored/config.toml;
  homelabConfig =
    if builtins.pathExists configPath then
      builtins.fromTOML (builtins.readFile configPath)
    else
      throw ''
        config.toml not found!
        Please create it from config.toml.example:
          cp ignored/config.toml.example ignored/config.toml
          # Edit ignored/config.toml with your settings
      '';

  cfg = homelabConfig.homelab or (throw "[homelab] section missing in config.toml");
  hostname = cfg.hostname or (throw "[homelab] hostname is required in config.toml");
  timezone = cfg.timezone or (throw "[homelab] timezone is required in config.toml");
  locale = cfg.locale or (throw "[homelab] locale is required in config.toml");
  sshPort = cfg.ssh_port or (throw "[homelab] ssh_port is required in config.toml");
  allowedSshKeys =
    cfg.allowed_ssh_keys or (throw "[homelab] allowed_ssh_keys is required in config.toml");
  rootSshKey = cfg.root_ssh_key or "";
  homelabPasswordHash = cfg.homelab_password_hash or "";
in
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
  ]
  ++ lib.optional (builtins.pathExists ../ignored/hardware-configuration.nix) ../ignored/hardware-configuration.nix;
  boot.loader.systemd-boot.enable = true;

  time.timeZone = timezone;
  i18n.defaultLocale = locale;

  networking = {
    hostName = hostname;
    networkmanager.enable = true;
    enableIPv6 = true;
    firewall.enable = false;
  };


  services.openssh = {
    enable = true;
    ports = [ sshPort ];
    settings = {
      PermitRootLogin = lib.mkIf (rootSshKey != "") "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  users.users.root.openssh.authorizedKeys.keys = lib.optional (rootSshKey != "") rootSshKey;

  users.users.homelab = {
    isNormalUser = true;
    extraGroups = [
      "wheel"
      "networkmanager"
      "docker"
    ];
    openssh.authorizedKeys.keys = allowedSshKeys;
    hashedPassword = lib.mkIf (homelabPasswordHash != "") homelabPasswordHash;
  };

  # Sudo requires password (default behavior, wheelNeedsPassword = true)
  virtualisation.docker.enable = true;
  services.logind.lidSwitchExternalPower = "ignore";

  environment.systemPackages =
    with pkgs;
    map lib.lowPrio [
      curl
      gitMinimal
      just
      nginx
      wireguard-tools
      docker
      docker-compose
      dysk
      ctop
      vim
      wget
      htop
      tmux
      frp
    ];

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  nix.gc.automatic = true;

  systemd.tmpfiles.rules = [
    "d /etc/yolab 0755 root root -"
    "d /var/lib/yolab 0755 root root -"
    "d /var/lib/yolab/services 0755 root root -"
  ];

  environment.etc."yolab/config.toml".source = configPath;

  system.stateVersion = "24.05";
}
