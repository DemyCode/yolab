{ modulesPath, config, pkgs, lib, ... }:

let
  # Load machine-specific configuration from TOML
  configPath = ./config.toml;
  homelabConfig = if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else throw "config.toml not found! Please create it from config.toml.example";

  # Extract configuration values with defaults
  cfg = homelabConfig.homelab or {};
  hostname = cfg.hostname or "homelab";
  timezone = cfg.timezone or "UTC";
  locale = cfg.locale or "en_US.UTF-8";
  sshPort = cfg.ssh_port or 22;
  allowedSshKeys = cfg.allowed_ssh_keys or [];
  rootSshKey = cfg.root_ssh_key or "";

  # Docker compose configuration
  dockerCfg = homelabConfig.docker or {};
  dockerEnabled = dockerCfg.enabled or false;
  dockerComposeUrl = dockerCfg.compose_url or "";

  # WiFi configuration (if configured during install)
  wifiCfg = homelabConfig.wifi or {};
  wifiSSID = wifiCfg.ssid or "";
  wifiPSK = wifiCfg.psk or "";

in {
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
  ] ++ lib.optional (builtins.pathExists ./hardware-configuration.nix) ./hardware-configuration.nix;

  # System hostname
  networking.hostName = hostname;

  # Bootloader configuration with disko support
  boot.loader.grub = {
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  # Timezone and locale
  time.timeZone = timezone;
  i18n.defaultLocale = locale;

  # Enable networking
  networking.networkmanager.enable = true;

  # Enable IPv6
  networking.enableIPv6 = true;

  # Disable firewall (like user's homelab)
  networking.firewall.enable = false;

  # Configure WiFi if provided during installation
  networking.wireless.networks = lib.mkIf (wifiSSID != "") {
    "${wifiSSID}".psk = wifiPSK;
  };

  # Enable SSH for both root and regular user
  services.openssh = {
    enable = true;
    ports = [ sshPort ];
    settings = {
      PermitRootLogin = lib.mkIf (rootSshKey != "") "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  # Root SSH key (if provided)
  users.users.root.openssh.authorizedKeys.keys = lib.optional (rootSshKey != "") rootSshKey;

  # Configure homelab user
  users.users.homelab = {
    isNormalUser = true;
    extraGroups = [ "wheel" "networkmanager" "docker" ];
    openssh.authorizedKeys.keys = allowedSshKeys;
  };

  # Enable sudo for wheel group
  security.sudo.wheelNeedsPassword = false;

  # Enable Docker
  virtualisation.docker.enable = true;

  # Ignore lid switch on external power
  services.logind.lidSwitchExternalPower = "ignore";

  # Essential packages for homelab
  environment.systemPackages = with pkgs; map lib.lowPrio [
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

  # Enable Nix flakes and commands
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  # Enable automatic garbage collection
  nix.gc.automatic = true;

  # Docker Compose deployment service (will be configured by homelab-setup)
  systemd.services.homelab-docker-compose = lib.mkIf dockerEnabled {
    script = ''
      mkdir -p /deployments
      cd /deployments

      # Download docker-compose.yml if URL is provided
      if [ -n "${dockerComposeUrl}" ]; then
        echo "Downloading docker-compose.yml from ${dockerComposeUrl}"
        ${pkgs.curl}/bin/curl -fsSL "${dockerComposeUrl}" -o docker-compose.yml
      fi

      if [ -f docker-compose.yml ]; then
        echo "Starting docker-compose services"
        ${pkgs.docker-compose}/bin/docker-compose up --build --remove-orphans --detach
        ${pkgs.docker}/bin/docker system prune --all --force
      else
        echo "No docker-compose.yml found, skipping deployment"
      fi
    '';
    path = [
      pkgs.docker-compose
      pkgs.docker
      pkgs.curl
    ];
    wantedBy = [ "multi-user.target" ];
    after = [
      "docker.service"
      "docker.socket"
    ];
    requires = [ "docker.service" ];
  };

  # System state version
  system.stateVersion = "24.05";
}
