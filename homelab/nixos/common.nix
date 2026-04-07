{
  pkgs,
  lib,
  inputs,
  ...
}:
let
  s = import ../shared.nix { inherit pkgs lib inputs; };
  k3sCfg = s.nodeCfg.k3s;
  isFirstNode = k3sCfg.server_addr == "";
  tunnelDomain = lib.removePrefix "https://" (lib.removePrefix "http://" s.tunnelCfg.dns_url);
in
{
  config = {
    time.timeZone = s.timezone;
    i18n.defaultLocale = s.locale;

    networking = {
      hostName = s.hostname;
      enableIPv6 = true;
      firewall.enable = false;

      wireguard.interfaces.wg0 = {
        ips = [
          "${s.tunnelCfg.sub_ipv6}/128"
          "${s.tunnelCfg.sub_ipv6_private}/128"
        ];
        privateKey = s.tunnelCfg.wg_private_key;
        postSetup = ''
          ip -6 route del ::/0 dev wg0 2>/dev/null || true
          ip -6 rule add from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
          ip -6 rule add from ${s.tunnelCfg.sub_ipv6_private} lookup 51820 priority 101 2>/dev/null || true
          ip -6 route add ::/0 dev wg0 table 51820
        '';
        preShutdown = ''
          ip -6 rule del from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
          ip -6 rule del from ${s.tunnelCfg.sub_ipv6_private} lookup 51820 priority 101 2>/dev/null || true
          ip -6 route del ::/0 dev wg0 table 51820 2>/dev/null || true
        '';
        peers = [
          {
            publicKey = s.tunnelCfg.wg_server_public_key;
            endpoint = s.tunnelCfg.wg_server_endpoint;
            allowedIPs = [ "::/0" ];
            persistentKeepalive = 25;
          }
        ];
      };
    };

    services.openssh = {
      enable = true;
      ports = [ s.sshPort ];
      settings = {
        PermitRootLogin = lib.mkIf (s.rootSshKey != "") "prohibit-password";
        PasswordAuthentication = false;
      };
    };

    boot.kernelModules = [
      "wireguard"
      "ip6_tables"
      "ip6table_filter"
      "iptable_nat"
      "xt_conntrack"
    ];

    services.k3s = {
      enable = true;
      role = "server";
      token = k3sCfg.token;
      clusterInit = isFirstNode;
      serverAddr = lib.optionalString (!isFirstNode) k3sCfg.server_addr;
      extraFlags = toString [
        "--flannel-backend=wireguard-native"
        "--advertise-address=${s.tunnelCfg.sub_ipv6_private}"
        "--node-ip=${s.tunnelCfg.sub_ipv6_private}"
      ];
    };

    services.caddy = {
      enable = true;
      configFile = pkgs.writeText "Caddyfile" ''
        ${tunnelDomain} {
          handle /api/* {
            reverse_proxy [::1]:3001
          }
          handle {
            root * ${s.clientUi}
            try_files {path} /index.html
            file_server
          }
        }
      '';
    };

    systemd.services.caddy = {
      after = [ "wireguard-wg0.service" ];
      wants = [ "wireguard-wg0.service" ];
    };

    systemd.services.yolab-local-api = {
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];
      environment = {
        PATH = lib.mkForce "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin";
        YOLAB_REPO_PATH = "/etc/nixos";
        YOLAB_CONFIG = "/etc/nixos/homelab/ignored/config.toml";
        YOLAB_PLATFORM = "nixos";
        YOLAB_FLAKE_TARGET = "yolab";
        YOLAB_NODE_IPV6 = s.tunnelCfg.sub_ipv6_private;
        KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "5s";
        ExecStart = "${s.localApiEnv}/bin/local-api";
      };
    };

    users.users.root.openssh.authorizedKeys.keys = lib.optional (s.rootSshKey != "") s.rootSshKey;

    users.users.homelab = {
      isNormalUser = true;
      extraGroups = [ "wheel" ];
      openssh.authorizedKeys.keys = s.allowedSshKeys;
      hashedPassword = lib.mkIf (s.homelabPasswordHash != "") s.homelabPasswordHash;
    };

    fileSystems = lib.listToAttrs (
      map (m: {
        name = m.path;
        value = {
          device = m.device;
          fsType = "auto";
          options = [
            "nofail"
            "x-systemd.device-timeout=10"
          ];
        };
      }) (s.nodeCfg.mounts or [ ])
    );

    services.nfs.server = {
      enable = true;
    };
    services.logind.lidSwitchExternalPower = "ignore";

    environment.systemPackages =
      with pkgs;
      map lib.lowPrio [
        curl
        gitMinimal
        just
        wireguard-tools
        kubectl
        nfs-utils
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
    services.swapspace.enable = true;
  };
}
