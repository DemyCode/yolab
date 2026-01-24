{ modulesPath, pkgs, lib, ... }:

let
  # Read config.toml from ignored/ directory (relative path)
  configPath = ../ignored/config.toml;
  homelabConfig =
    if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else throw ''
      config.toml not found!
      Please create it from config.toml.example:
        cp ignored/config.toml.example ignored/config.toml
        # Edit ignored/config.toml with your settings
    '';

  # Parse config sections
  cfg = homelabConfig.homelab or (throw "[homelab] section missing in config.toml");
  hostname = cfg.hostname or (throw "[homelab] hostname is required in config.toml");
  timezone = cfg.timezone or (throw "[homelab] timezone is required in config.toml");
  locale = cfg.locale or (throw "[homelab] locale is required in config.toml");
  sshPort = cfg.ssh_port or (throw "[homelab] ssh_port is required in config.toml");
  allowedSshKeys = cfg.allowed_ssh_keys or (throw "[homelab] allowed_ssh_keys is required in config.toml");
  rootSshKey = cfg.root_ssh_key or "";

  dockerCfg = homelabConfig.docker or (throw "[docker] section missing in config.toml");
  dockerEnabled = dockerCfg.enabled or (throw "[docker] enabled is required in config.toml");
  dockerComposeUrl = dockerCfg.compose_url or "";

  wifiCfg = homelabConfig.wifi or { };
  wifiEnabled = wifiCfg.enabled or false;
  wifiSSID = wifiCfg.ssid or "";
  wifiPSK = wifiCfg.psk or "";

in
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
  ] ++ lib.optional (builtins.pathExists ../ignored/hardware-configuration.nix) ../ignored/hardware-configuration.nix;

  boot.loader.grub = {
    efiSupport = true;
    efiInstallAsRemovable = true;
  };

  time.timeZone = timezone;
  i18n.defaultLocale = locale;

  networking = {
    hostName = hostname;
    networkmanager.enable = true;
    enableIPv6 = true;
    firewall.enable = false;
  };

  # WiFi configuration using NetworkManager (if enabled)
  systemd.services.yolab-wifi-setup = lib.mkIf (wifiEnabled && wifiSSID != "" && wifiPSK != "") {
    description = "Setup WiFi from config";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" ];
    path = [ pkgs.networkmanager ];
    serviceConfig = {
      Type = "oneshot";
      RemainAfterExit = true;
    };
    script = ''
      SSID="${wifiSSID}"
      PSK="${wifiPSK}"

      # Check if connection already exists
      if ! nmcli connection show "$SSID" &>/dev/null; then
        echo "Creating WiFi connection for $SSID"
        nmcli connection add \
          type wifi \
          con-name "$SSID" \
          ifname wlan0 \
          ssid "$SSID" \
          wifi-sec.key-mgmt wpa-psk \
          wifi-sec.psk "$PSK"
      else
        echo "WiFi connection $SSID already exists"
      fi
    '';
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
    extraGroups = [ "wheel" "networkmanager" "docker" ];
    openssh.authorizedKeys.keys = allowedSshKeys;
  };

  security.sudo.wheelNeedsPassword = false;
  virtualisation.docker.enable = true;
  services.logind.lidSwitchExternalPower = "ignore";

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

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  nix.gc.automatic = true;

  # Setup directories
  systemd.tmpfiles.rules = [
    "d /etc/yolab 0755 root root -"
    "d /var/lib/yolab 0755 root root -"
    "d /var/lib/yolab/services 0755 root root -"
  ];

  # Copy config.toml to /etc/yolab for runtime access
  environment.etc."yolab/config.toml".source = configPath;

  # Docker compose service
  systemd.services.homelab-docker-compose = lib.mkIf dockerEnabled {
    script = ''
      mkdir -p /deployments
      cd /deployments

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

  system.stateVersion = "24.05";
}
