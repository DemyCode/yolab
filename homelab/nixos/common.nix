{
  pkgs,
  lib,
  config,
  inputs,
  ...
}: let
  s = import ../shared.nix {inherit pkgs lib inputs;};
  k3sCfg = s.nodeCfg.k3s;

  # The first node initialises the embedded-etcd cluster (--cluster-init).
  # Every other node joins as an equal server peer via serverAddr.
  # After joining, all nodes are identical: control plane + worker + UI.
  isFirstNode = k3sCfg.server_addr == "";

  tunnelDomain = lib.removePrefix "https://" (lib.removePrefix "http://" s.tunnelCfg.dns_url);
in {
  # ── Module options ────────────────────────────────────────────────────────
  # Consumed by platform overlays (wsl.nix, darwin/configuration.nix …).
  # Defaults cover the standard bare-metal / QEMU case.
  options.yolab = {
    platform = lib.mkOption {
      type = lib.types.str;
      default = "nixos";
      description = "Platform identifier forwarded to local-api (nixos, wsl, …).";
    };
    flakeTarget = lib.mkOption {
      type = lib.types.str;
      default = "yolab";
      description = "Flake output name used by nixos-rebuild switch.";
    };
    repoPath = lib.mkOption {
      type = lib.types.str;
      default = "/etc/nixos";
      description = "Absolute path to the yolab repo on this machine.";
    };
  };

  config = {
    time.timeZone = s.timezone;
    i18n.defaultLocale = s.locale;

    # ── DNS ───────────────────────────────────────────────────────────────
    # Point the node itself at IPv6-capable public resolvers.
    # The same servers are written to /etc/k3s-resolv.conf so that CoreDNS
    # and kubelet use them as upstreams — essential on an IPv6-only host.
    networking.nameservers = [
      "2606:4700:4700::1111" # Cloudflare
      "2001:4860:4860::8888" # Google
    ];

    environment.etc."k3s-resolv.conf".text = ''
      nameserver 1.1.1.1
      nameserver 8.8.8.8
      nameserver 2606:4700:4700::1111
      nameserver 2001:4860:4860::8888
    '';

    # ── Networking ────────────────────────────────────────────────────────
    networking = {
      hostName = s.hostname;
      enableIPv6 = true;
      firewall.enable = false;

      # ── WireGuard ──────────────────────────────────────────────────────
      #
      # Topology: hub-and-spoke.  Every node has ONE peer — the external
      # WireGuard server in yolab-external.  The server relays traffic
      # between nodes (Node A → hub → Node B).  New nodes appear on the
      # hub automatically via the wireguard-manager daemon; existing nodes
      # never need a rebuild when the cluster grows.
      #
      # Each node gets two addresses on wg0:
      #   sub_ipv6         – public, routed by the external DNS server.
      #                      Caddy binds here to serve the management UI.
      #   sub_ipv6_private – private cluster IP used by K3s, Flannel VXLAN,
      #                      kubelet, and the local-api fan-out calls.
      #
      # Routing strategy — two complementary rules:
      #
      #  A. Destination route (main table):
      #       ip -6 route add <privateSubnet> dev wg0
      #     Any packet headed for another node's cluster IP exits wg0,
      #     regardless of source.  This is what makes VXLAN and kubelet
      #     traffic work — those sockets may use a source address that the
      #     source-based policy rule below wouldn't catch.
      #
      #  B. Source policy (table 51820):
      #       ip -6 rule add from <our IPs> lookup 51820
      #       ip -6 route add ::/0 dev wg0 table 51820
      #     Return / keepalive / outbound traffic originating from our own
      #     WireGuard addresses also exits wg0, preventing asymmetric routing
      #     for inbound tunnel connections.
      wireguard.interfaces.wg0 = {
        ips = [
          "${s.tunnelCfg.sub_ipv6}/128"
          "${s.tunnelCfg.sub_ipv6_private}/128"
        ];
        privateKey = s.tunnelCfg.wg_private_key;

        postSetup = ''
          # A. Destination route: all cluster-node IPs go through wg0
          ip -6 route replace ${s.privateSubnet} dev wg0 2>/dev/null || true

          # B. Source policy: sub_ipv6 (public/Caddy address) always exits through wg0.
          #    sub_ipv6_private is NOT added here — it is a ULA address only reachable
          #    within the fd00:cafe::/112 cluster subnet, which is already covered by
          #    the destination route above (rule A).  Adding a source policy for
          #    sub_ipv6_private sends API-server replies to pods via wg0 instead of
          #    the local bridge, causing i/o timeouts for all in-cluster service traffic.
          ip -6 rule add from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
          ip -6 route replace ::/0 dev wg0 table 51820 2>/dev/null || true

          # C. Default route in main table: allows pod traffic (fd00:42::/56) to reach
          #    external IPv6 via wg0.  Flannel's --flannel-ipv6-masq SNATs the pod source
          #    to sub_ipv6, which is then picked up by source policy B above.  This is
          #    needed so app WireGuard sidecars can reach the hub to establish their tunnel.
          #    metric 200 loses to any ISP-provided default route (single encapsulation path)
          #    and wins only when no ISP IPv6 exists (double encapsulation path, still works).
          ip -6 route replace ::/0 dev wg0 metric 200 2>/dev/null || true
        '';

        preShutdown = ''
          ip -6 route del ${s.privateSubnet} dev wg0 2>/dev/null || true
          ip -6 rule del from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
          ip -6 route del ::/0 dev wg0 table 51820 2>/dev/null || true
          ip -6 route del ::/0 dev wg0 metric 200 2>/dev/null || true
        '';

        peers = [
          {
            # The external hub is the only peer on every node.
            # It knows about all registered nodes and relays traffic between them.
            publicKey = s.tunnelCfg.wg_server_public_key;
            endpoint = s.tunnelCfg.wg_server_endpoint;
            allowedIPs = ["::/0"];
            persistentKeepalive = 25;
          }
        ];
      };
    };

    # ── SSH ───────────────────────────────────────────────────────────────
    services.openssh = {
      enable = true;
      ports = [s.sshPort];
      settings = {
        PermitRootLogin = "prohibit-password";
        PasswordAuthentication = false;
      };
    };

    # ── Kernel ────────────────────────────────────────────────────────────
    boot.kernelModules = [
      "wireguard"
      "ip6_tables"
      "ip6table_filter"
      "ip6table_nat"
      "iptable_nat"
      "xt_conntrack"
      "br_netfilter"
      "overlay"
      "nf_nat"
      "ceph"
    ];

    boot.kernel.sysctl = {
      "net.bridge.bridge-nf-call-iptables" = 1;
      "net.bridge.bridge-nf-call-ip6tables" = 1;
      "net.ipv4.ip_forward" = 1;
      "net.ipv6.conf.all.forwarding" = 1;
      # Keep Ceph daemons in RAM — they perform poorly when swapped out.
      "vm.swappiness" = 10;
      "vm.dirty_ratio" = 40;
      "vm.dirty_background_ratio" = 10;
    };

    # ── K3s ───────────────────────────────────────────────────────────────
    #
    # Every node runs as a K3s *server* (control plane + worker).
    # Apps can be scheduled on any node; the cluster is HA once there are
    # 3+ nodes (embedded etcd quorum = n/2 + 1).
    #
    # Flannel backend: vxlan — NOT wireguard-native.
    #   wg0 already encrypts all inter-node traffic end-to-end.
    #   wireguard-native would add a second WireGuard layer on top (double
    #   encapsulation, ~2× overhead, more complex routing).  With vxlan, pod
    #   traffic is encapsulated then encrypted once by wg0 — simpler and faster.
    #
    # --cluster-dns: the 10th address of the service CIDR (fd00:43::a).
    #   K3s normally infers this, but we set it explicitly because the
    #   auto-inference can silently pick the wrong address with a custom
    #   IPv6-only CIDR.
    #
    # --tls-san: adds sub_ipv6_private to the API-server TLS certificate.
    #   Without this, joining nodes get a certificate mismatch when they
    #   connect to https://[sub_ipv6_private]:6443.
    #
    # --advertise-address: tells the API server which address to advertise
    #   to the rest of the cluster — must be the private cluster IP so that
    #   other nodes (via the hub relay) can reach it.
    services.k3s = {
      enable = true;
      role = "server";
      inherit (k3sCfg) token;
      clusterInit = isFirstNode;
      serverAddr = k3sCfg.server_addr; # "" on the first node — K3s ignores it

      extraFlags = [
        # Traefik is not used — YoLab exposes apps via WireGuard sidecars and
        # Caddy handles the management UI.  Leaving Traefik enabled causes its
        # svclb DaemonSet to bind hostPorts 80/443 on every node, which
        # conflicts with Caddy and causes it to receive SIGTERM.
        "--disable=traefik"
        "--flannel-backend=vxlan"
        "--flannel-ipv6-masq"
        "--cluster-cidr=fd00:42::/56,10.42.0.0/16"
        "--service-cidr=fd00:43::/112,10.43.0.0/16"
        "--cluster-dns=fd00:43::a"
        "--advertise-address=${s.tunnelCfg.sub_ipv6_private}"
        "--tls-san=${s.tunnelCfg.sub_ipv6_private}"
        "--resolv-conf=/etc/k3s-resolv.conf"
      ];
    };

    # Detect the node's outbound IPv4 at boot and write it to K3s's config file
    # as node-ip alongside the private IPv6, enabling dual-stack pods.
    # Running before K3s and after WireGuard ensures the IPv6 address is up.
    systemd.services.k3s-node-ip = {
      description = "Write K3s dual-stack node-ip config";
      after = [
        "wireguard-wg0.service"
        "network-online.target"
      ];
      wants = ["network-online.target"];
      before = ["k3s.service"];
      wantedBy = ["k3s.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "k3s-node-ip" ''
          IPV4=$(${pkgs.iproute2}/bin/ip -4 route get 1.1.1.1 2>/dev/null | grep -oP 'src \K\S+' || true)
          mkdir -p /etc/rancher/k3s
          if [ -n "$IPV4" ]; then
            echo "node-ip: ${s.tunnelCfg.sub_ipv6_private},$IPV4" > /etc/rancher/k3s/config.yaml
          else
            echo "node-ip: ${s.tunnelCfg.sub_ipv6_private}" > /etc/rancher/k3s/config.yaml
          fi
        '';
      };
    };

    # K3s must start after WireGuard so the node-ip is reachable before K3s
    # tries to register itself with the cluster.
    systemd.services.k3s = {
      after = [
        "wireguard-wg0.service"
        "k3s-node-ip.service"
      ];
      wants = ["wireguard-wg0.service"];
      serviceConfig.TimeoutStopSec = "30";
    };

    # ── Caddy ─────────────────────────────────────────────────────────────
    # Serves the management UI over HTTPS on the node's public tunnel address.
    # Caddy is the only service that needs the public sub_ipv6.
    # Everything else — app installs, kubectl, inter-node API calls — travels
    # over private WireGuard addresses inside the cluster subnet.
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
      after = ["wireguard-wg0.service"];
      wants = ["wireguard-wg0.service"];
    };

    # ── System-disk OSD ───────────────────────────────────────────────────────
    # Creates a sparse file on the root filesystem (no partitioning) and
    # attaches it as a loop device.  On first boot the file is sized to 25%
    # of the root filesystem capacity.
    systemd.services.yolab-system-osd = {
      description = "System-disk Ceph OSD (loop-file)";
      wantedBy = ["multi-user.target"];
      after = ["local-fs.target"];
      before = ["k3s.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "system-osd-start" ''
          set -euo pipefail
          IMG=/var/lib/rook/system-osd.img

          # Size the image to 75% of root-filesystem capacity.
          # fallocate pre-allocates real disk blocks (no sparse regions) so
          # BlueStore label writes always land on already-allocated blocks.
          # This prevents the partial-write corruption that sparse files cause
          # when block allocation is interrupted mid-write.
          mkdir -p /var/lib/rook
          set -- $(${pkgs.coreutils}/bin/df -B1 / | tail -1)
          TARGET=$(( $2 * 3 / 4 ))
          if [ ! -f "$IMG" ]; then
            ${pkgs.util-linux}/bin/fallocate -l "$TARGET" "$IMG"
          else
            CURRENT=$(${pkgs.coreutils}/bin/stat -c%s "$IMG")
            if [ "$CURRENT" -lt "$TARGET" ]; then
              ${pkgs.util-linux}/bin/fallocate -l "$TARGET" "$IMG"
              # Notify the running loop driver of the new size (no-op if not attached).
              ${pkgs.util-linux}/bin/losetup -c /dev/loop0 2>/dev/null || true
            fi
          fi

          # Attach to /dev/loop0 so the device name is stable across reboots.
          # --direct-io=on bypasses the page cache for the loop device — Ceph
          # manages its own cache, so OS-level caching only adds overhead and
          # creates double-buffering consistency risks with the backing filesystem.
          ATTACHED=$(${pkgs.util-linux}/bin/losetup -j "$IMG" 2>/dev/null | grep "^/dev/loop0:" || true)
          if [ -z "$ATTACHED" ]; then
            ${pkgs.util-linux}/bin/losetup -d /dev/loop0 2>/dev/null || true
            ${pkgs.util-linux}/bin/losetup --direct-io=on /dev/loop0 "$IMG"
          fi
        '';
        ExecStop = pkgs.writeShellScript "system-osd-stop" ''
          LOOP=$(${pkgs.util-linux}/bin/losetup -j /var/lib/rook/system-osd.img 2>/dev/null \
                 | ${pkgs.coreutils}/bin/cut -d: -f1)
          [ -n "$LOOP" ] && ${pkgs.util-linux}/bin/losetup -d "$LOOP" || true
        '';
      };
    };

    # ── Local API ──────────────────────────────────────────────────────────
    # Runs on every node.  The node the user opens in their browser queries
    # its own local-api, which fans out disk / storage / node requests to
    # sibling nodes via their private IPv6 addresses (discovered from kubectl).
    systemd.services.yolab-local-api = {
      after = [
        "network.target"
        "k3s.service"
      ];
      wants = ["k3s.service"];
      wantedBy = ["multi-user.target"];
      environment = {
        PATH = lib.mkForce "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin";
        YOLAB_REPO_PATH = config.yolab.repoPath;
        YOLAB_CONFIG = "${config.yolab.repoPath}/homelab/ignored/config.toml";
        YOLAB_PLATFORM = config.yolab.platform;
        YOLAB_FLAKE_TARGET = config.yolab.flakeTarget;
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

    # ── Users ─────────────────────────────────────────────────────────────
    users.users.root.openssh.authorizedKeys.keys =
      lib.optional (s.rootSshKey != "") s.rootSshKey
      ++ [ "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK4KqHP17dqZURgVG7NwJ4sRoPVpmmNb3fMhGiWD529z nixos@nixos" ];

    users.users.homelab = {
      isNormalUser = true;
      extraGroups = ["wheel"];
      openssh.authorizedKeys.keys = s.allowedSshKeys;
      hashedPassword = lib.mkIf (s.homelabPasswordHash != "") s.homelabPasswordHash;
    };

    services.logind.settings.Login.HandleLidSwitchExternalPower = "ignore";

    environment.systemPackages = with pkgs;
      map lib.lowPrio [
        curl
        gitMinimal
        just
        wireguard-tools
        kubectl
        gptfdisk # sgdisk — wipes disks before Rook claims them
        dysk
        dust
        ctop
        vim
        wget
        htop
      ];

    # ── Rook / Ceph ───────────────────────────────────────────────────────────
    # K3s watches /var/lib/rancher/k3s/server/manifests/ and auto-applies
    # any YAML placed there.  Symlinks into the Nix store so updates
    # propagate on nixos-rebuild without manual kubectl apply.
    systemd.tmpfiles.rules = [
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-operator.yaml  - - - - ${./rook/operator.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-cluster.yaml   - - - - ${./rook/cluster.yaml}"
    ];

    system.activationScripts.yolabVersion = ''
      mkdir -p /var/lib/yolab
      ${pkgs.git}/bin/git -C ${config.yolab.repoPath} rev-parse HEAD        > /var/lib/yolab/built-hash    2>/dev/null || true
      ${pkgs.git}/bin/git -C ${config.yolab.repoPath} log -1 --pretty=%s    > /var/lib/yolab/built-message 2>/dev/null || true
      ${pkgs.git}/bin/git -C ${config.yolab.repoPath} log -1 --pretty=%cI   > /var/lib/yolab/built-date    2>/dev/null || true
    '';

    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    nix.gc.automatic = true;

    swapDevices = [{
      device = "/var/lib/swapfile";
      size = 8192;
    }];
    services.swapspace.enable = true;
  };
}
