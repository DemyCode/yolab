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

    # boot.kernelModules = [
    #   "br_netfilter"
    #   "overlay"
    #   "ip6_tables"
    #   "ip6table_filter"
    #   "ip6table_nat"
    #   "iptable_nat"
    #   "xt_conntrack"
    # ];

    # boot.kernel.sysctl = {
    #   "net.bridge.bridge-nf-call-iptables" = 1;
    #   "net.bridge.bridge-nf-call-ip6tables" = 1;
    #   "net.ipv4.ip_forward" = 1;
    #   "net.ipv6.conf.all.forwarding" = 1;
    # };

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

    # environment.etc."k3s-resolv.conf".text = ''
    #   nameserver 2606:4700:4700::1111
    #   nameserver 2001:4860:4860::8888
    # '';

    services.k3s = {
      enable = true;
      role = "server";
      token = k3sCfg.token;
      clusterInit = isFirstNode;
      serverAddr = lib.optionalString (!isFirstNode) k3sCfg.server_addr;
      # extraFlags = toString [
      #   "--flannel-backend=vxlan"
      #   "--flannel-ipv6-masq"
      #   "--cluster-cidr=fd00:42::/56"
      #   "--service-cidr=fd00:43::/112"
      #   "--node-ip=${s.tunnelCfg.sub_ipv6_private}"
      #   "--advertise-address=${s.tunnelCfg.sub_ipv6_private}"
      #   "--bind-address=::"
      #   "--resolv-conf=/etc/k3s-resolv.conf"
      #   "--disable=traefik"
      # ];
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

    services.nfs.server.enable = true;

    # systemd.services.yolab-nfs-restore = {
    #   description = "Restore YoLab NFS exports";
    #   after = [ "nfs-server.service" ];
    #   wantedBy = [ "multi-user.target" ];
    #   serviceConfig = {
    #     Type = "oneshot";
    #     ExecStart = "${pkgs.nfs-utils}/bin/exportfs -ra";
    #     RemainAfterExit = true;
    #   };
    # };

    services.logind.lidSwitchExternalPower = "ignore";

    environment.systemPackages =
      with pkgs;
      map lib.lowPrio [
        curl
        gitMinimal
        wireguard-tools
        kubectl
        nfs-utils
        iptables
        vim
        htop
        dysk
        dust
        ctop
      ];

    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    nix.gc.automatic = true;
    services.swapspace.enable = true;
  };
}
