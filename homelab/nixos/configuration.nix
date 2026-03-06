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

  tunnelCfg = homelabConfig.tunnel or { };
  tunnelEnabled = tunnelCfg.enabled or false;
  # Derive the /64 prefix from sub_ipv6 (e.g. "2a01:4f8:1c19:b6ce::42" → "2a01:4f8:1c19:b6ce::/64")
  # Only route traffic within the WireGuard subnet through the tunnel — no default route.
  wgSubnet = lib.optionalString tunnelEnabled (
    (lib.head (lib.splitString "::" tunnelCfg.sub_ipv6)) + "::/64"
  );

  clientUi = pkgs.buildNpmPackage {
    pname = "client-ui";
    version = "0.1.0";
    src = ../client-ui;
    npmDepsHash = "sha256-vB4y/Ct1i7An5uP6fTEUwEYhjZApT6ZpLMq3cs996NY=";
    installPhase = ''
      npm run build
      cp -r dist $out
    '';
  };
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

    wireguard.interfaces = lib.mkIf tunnelEnabled {
      wg0 = {
        ips = [ "${tunnelCfg.sub_ipv6}/128" ];
        privateKey = tunnelCfg.wg_private_key;
        peers = [
          {
            publicKey = tunnelCfg.wg_server_public_key;
            endpoint = tunnelCfg.wg_server_endpoint;
            allowedIPs = [ wgSubnet ];
            persistentKeepalive = 25;
          }
        ];
      };
    };
  };

  services.openssh = {
    enable = true;
    ports = [ sshPort ];
    settings = {
      PermitRootLogin = lib.mkIf (rootSshKey != "") "prohibit-password";
      PasswordAuthentication = false;
    };
  };

  services.avahi = {
    enable = true;
    nssmdns4 = true;
    publish = {
      enable = true;
      addresses = true;
      domain = true;
    };
  };

  services.nginx = {
    enable = true;
    virtualHosts."default" = {
      default = true;
      listen = [
        {
          addr = "0.0.0.0";
          port = 80;
        }
        {
          addr = "[::]";
          port = 80;
        }
      ]
      ++ lib.optionals tunnelEnabled [
        {
          addr = "[${tunnelCfg.sub_ipv6}]";
          port = 80;
        }
      ];
      root = "${clientUi}";
      locations."/" = {
        tryFiles = "$uri $uri/ /index.html";
      };
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
      dust
      ctop
      vim
      wget
      htop
    ];

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
  nix.gc.automatic = true;
  system.stateVersion = "24.05";
}
