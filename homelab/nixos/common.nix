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
  # Other nodes' sub_ipv6_private addresses — one entry per cluster machine.
  # Each becomes an explicit /128 route via wg0 so flannel vxlan can reach it.
  # Add a peer's IP here and run nixos-rebuild when a new machine joins.
  swarmPeers = s.swarmCfg.peers or [ ];
in
{
  config = {
    time.timeZone = s.timezone;
    i18n.defaultLocale = s.locale;

    # -------------------------------------------------------------------------
    # Networking & WireGuard
    #
    # Three planes share wg0:
    #   1. Management  — inbound to sub_ipv6 (public), served by Caddy
    #   2. k3s cluster — inbound/outbound to sub_ipv6_private (ULA, cluster only)
    #   3. Node internet — goes via the physical NIC, never through wg0
    #
    # AllowedIPs = ::/0 so WireGuard accepts all inbound (internet clients have
    # arbitrary IPs). The NixOS module auto-adds "default via wg0"; postSetup
    # removes it and builds the routing table we actually want.
    # -------------------------------------------------------------------------
    networking = {
      hostName = s.hostname;
      enableIPv6 = true;
      firewall.enable = false;
      # nameservers = [ "2606:4700:4700::1111" "2001:4860:4860::8888" ];

      wireguard.interfaces.wg0 = {
        # ips = [
        #   "${s.tunnelCfg.sub_ipv6}/128" # public — management UI
        #   "${s.tunnelCfg.sub_ipv6_private}/128" # private — k3s node IP
        # ];
        privateKey = s.tunnelCfg.wg_private_key;
        peers = [
          {
            publicKey = s.tunnelCfg.wg_server_public_key;
            endpoint = s.tunnelCfg.wg_server_endpoint;
            allowedIPs = [ "::/0" ];
            persistentKeepalive = 25;
          }
        ];

        # postSetup = ''
        #   # Remove the default route NixOS adds for AllowedIPs ::/0.
        #   # Node internet traffic must stay on the physical NIC.
        #   ip -6 route del ::/0 dev wg0 2>/dev/null || true
        #
        #   # Management plane: Caddy responds from sub_ipv6 so responses must
        #   # go back through the tunnel (sub_ipv6 is only routable via the server).
        #   ip -6 rule add from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
        #   ip -6 route add ::/0 dev wg0 table 51820
        #
        #   # k3s cluster plane: explicit route to each peer's private IP so
        #   # flannel vxlan outer packets reach the right node via the tunnel.
        #   ${lib.concatMapStrings (ip: ''
        #     ip -6 route add ${ip}/128 dev wg0 2>/dev/null || true
        #   '') swarmPeers}
        # '';

        # preShutdown = ''
        #   ip -6 rule del from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
        #   ip -6 route del ::/0 dev wg0 table 51820 2>/dev/null || true
        #   ${lib.concatMapStrings (ip: ''
        #     ip -6 route del ${ip}/128 dev wg0 2>/dev/null || true
        #   '') swarmPeers}
        # '';
      };
    };

    # -------------------------------------------------------------------------
    # SSH
    # -------------------------------------------------------------------------
    services.openssh = {
      enable = true;
      ports = [ s.sshPort ];
      settings = {
        PermitRootLogin = lib.mkIf (s.rootSshKey != "") "prohibit-password";
        PasswordAuthentication = false;
      };
    };

    # -------------------------------------------------------------------------
    # Kernel — modules required for k3s + WireGuard + IPv6
    # -------------------------------------------------------------------------
    # boot.kernelModules = [
    #   "wireguard" # WireGuard VPN
    #   "br_netfilter" # bridge traffic through netfilter (required by k3s)
    #   "overlay" # OverlayFS for container images (required by k3s)
    #   "ip6_tables" # IPv6 iptables framework
    #   "ip6table_filter" # IPv6 FILTER table
    #   "ip6table_nat" # IPv6 NAT (service ClusterIP DNAT)
    #   "iptable_nat" # IPv4 NAT (k3s internal use)
    #   "xt_conntrack" # connection tracking (required for NAT)
    # ];
    #
    # boot.kernel.sysctl = {
    #   "net.bridge.bridge-nf-call-iptables" = 1;
    #   "net.bridge.bridge-nf-call-ip6tables" = 1;
    #   "net.ipv4.ip_forward" = 1;
    #   "net.ipv6.conf.all.forwarding" = 1;
    # };

    # -------------------------------------------------------------------------
    # k3s
    #
    # flannel-backend=vxlan: pods communicate over a VXLAN overlay. The outer
    # UDP packets are routed to each node's sub_ipv6_private via wg0 (see
    # swarmPeers routes above). This avoids the "two WireGuard systems" conflict
    # that wireguard-native caused.
    #
    # kube-proxy runs in iptables mode (not nftables) — consistent with the
    # ip6table_nat modules loaded above.
    # -------------------------------------------------------------------------
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
      #   "--advertise-address=${s.tunnelCfg.sub_ipv6_private}"
      #   "--node-ip=${s.tunnelCfg.sub_ipv6_private}"
      #   "--bind-address=::"
      #   "--resolv-conf=/etc/k3s-resolv.conf"
      #   "--kube-proxy-arg=proxy-mode=iptables"
      # ];
    };

    # -------------------------------------------------------------------------
    # Management UI — Caddy serves the React app and proxies the local API.
    # Listens on all interfaces; only reachable externally via sub_ipv6 through
    # the WireGuard tunnel (the DNS for tunnelDomain points there).
    # -------------------------------------------------------------------------
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

    # -------------------------------------------------------------------------
    # Local API — FastAPI backend for the management UI.
    # Serves on [::]:3001 (loopback + cluster); Caddy proxies it externally.
    # -------------------------------------------------------------------------
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

    # -------------------------------------------------------------------------
    # Users
    # -------------------------------------------------------------------------
    users.users.root.openssh.authorizedKeys.keys = lib.optional (s.rootSshKey != "") s.rootSshKey;

    users.users.homelab = {
      isNormalUser = true;
      extraGroups = [ "wheel" ];
      openssh.authorizedKeys.keys = s.allowedSshKeys;
      hashedPassword = lib.mkIf (s.homelabPasswordHash != "") s.homelabPasswordHash;
    };

    # -------------------------------------------------------------------------
    # Storage — NFS server for app volumes (exports managed dynamically via
    # /etc/exports.d/yolab.exports written by the local API's disks router).
    # -------------------------------------------------------------------------
    services.nfs.server.enable = true;

    # Re-apply any persisted exports from /etc/exports.d/yolab.exports after boot.
    systemd.services.yolab-nfs-restore = {
      description = "Restore YoLab NFS exports";
      after = [ "nfs-server.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        ExecStart = "${pkgs.nfs-utils}/bin/exportfs -ra";
        RemainAfterExit = true;
      };
    };

    # Keep the machine running when the lid is closed (laptops used as servers).
    services.logind.lidSwitchExternalPower = "ignore";

    # -------------------------------------------------------------------------
    # System packages — only what the homelab actually needs at runtime.
    # -------------------------------------------------------------------------
    environment.systemPackages =
      with pkgs;
      map lib.lowPrio [
        curl # HTTP testing / health checks
        gitMinimal # nixos-rebuild fetches from git
        wireguard-tools # wg / wg-quick for diagnostics
        kubectl # cluster management
        nfs-utils # exportfs (disk storage management)
        iptables # kube-proxy iptables mode
        vim # editor
        htop # process monitor
        dysk # disk usage overview
        dust # du alternative
        ctop # container top
      ];

    # -------------------------------------------------------------------------
    # Nix
    # -------------------------------------------------------------------------
    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    nix.gc.automatic = true;
    services.swapspace.enable = true;
  };
}
