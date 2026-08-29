{
  pkgs,
  lib,
  config,
  inputs,
  yolabConfigPath,
  rust,
  ...
}: let
  s = import ../shared.nix {inherit pkgs lib inputs yolabConfigPath rust;};
  k3sCfg = s.nodeCfg.k3s;

  # The first node initialises the embedded-etcd cluster (--cluster-init).
  # Every other node joins as an equal server peer via serverAddr.
  # After joining, all nodes are identical: control plane + worker + UI.
  isFirstNode = k3sCfg.server_addr == "";

  tunnelDomain = lib.removePrefix "https://" (lib.removePrefix "http://" s.tunnelCfg.dns_url);

  # Ceph runs as host daemons rather than Rook pods so containerd's image store
  # can live on an RBD — see homelab/nixos/ceph/default.nix for why that is not
  # possible while the mons are pods.
  cephCfg = s.homelabConfig.ceph or {};

  # The mesh address of a machine already in the cluster, taken from the k3s
  # server URL the installer wrote — same tunnel, same peer, one fewer thing to
  # keep in sync. Empty on the machine that creates the cluster.
  #
  # A `throw` rather than a fallback to "": silently treating an unparseable
  # server_addr as "this is the first node" would make a joining machine
  # bootstrap its own Ceph cluster instead of joining, which looks healthy on
  # both nodes and is only discovered when the storage turns out to be split.
  cephSeedAddr =
    if isFirstNode
    then ""
    else let
      # `]` is deliberately unescaped outside the bracket expression: Nix uses POSIX
      # ERE, where `\]` is not a valid escape and the whole pattern is rejected at
      # eval time with "invalid regular expression".
      m = builtins.match "https?://\\[([^]]+)]:[0-9]+" k3sCfg.server_addr;
    in
      if m == null
      then throw "[ceph] cannot read a cluster address out of node.k3s.server_addr (${k3sCfg.server_addr}); expected https://[<ipv6>]:6443"
      else builtins.head m;
in {
  imports = [
    ./ceph
    ./ceph/images-store.nix
    ./ceph/filesystem.nix
    ./ceph/csi-secrets.nix
    ./ceph/maintenance.nix
    ./ceph/dashboard.nix
  ];

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
    # No glances. It was a web system monitor on :61208, proxied at /glances.
    #
    # It is gone because it could not be installed reliably. glances 4.5.5 is not
    # in cache.nixos.org (its narinfo is a 404), so every node compiled it from
    # source — running its test suite, two of whose REST tests are racy. That is
    # a coin flip per machine at install time, and it is what made the second
    # machine fail to install while the first, which had already won that toss
    # and kept the result in its store, kept working.
    #
    # An overlay used to patch those tests out. Removing it was right in itself —
    # overriding the derivation is what forced the from-source build in the first
    # place — but it was removed on the belief that a prebuilt 4.5.5 existed. It
    # does not. Bringing glances back means depending on a cached build, not
    # patching the tests again.

    # Ceph, and only Ceph, comes from a newer nixpkgs. The main pin's 20.2.2
    # fails its own python-common test suite, so Hydra never built it and it is
    # absent from the binary cache — leaving every node to compile Ceph from
    # source. 20.2.3 is fixed and cached. Scoped to two attributes so nothing
    # else in the closure moves.
    nixpkgs.overlays = [
      (_final: prev: {
        inherit (inputs.nixpkgs-ceph.legacyPackages.${prev.stdenv.hostPlatform.system}) ceph ceph-client;
      })
    ];

    # Ceph runs as host daemons, outside k3s — the only arrangement in which
    # containerd's image store can live on an RBD. See homelab/nixos/ceph/.
    yolab.ceph = {
      enable = true;
      fsid = cephCfg.fsid or (throw "[ceph] fsid is required in config.toml");
      monAddr = s.nodeCfg.sub_ipv6_private;
      # The mesh, not this node's /128 — see yolab.ceph.clusterSubnet.
      clusterSubnet = s.privateSubnet;
      # "" on the first machine; every other machine joins through this one.
      joinSeedAddr = cephSeedAddr;
      imagesStore.enable = true;
      # CephFS behind every app PVC, and the credentials ceph-csi needs to
      # reach it. Rook stays only to run CSI, in external mode.
      filesystem.enable = true;
      csiSecrets.enable = true;
      # noout across reboots, and a health gate the update path calls before it
      # restarts Ceph daemons.
      maintenance.enable = true;
    };

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
      # Topology: hub-and-spoke.  Every node connects to the external WireGuard
      # server in yolab-external via TWO independent interfaces, each with its
      # own keypair:
      #
      #   wg0 — tunnel interface (public address sub_ipv6 = 2a01::...)
      #     • Caddy binds here to serve the management UI over HTTPS.
      #     • Source-policy routing ensures outbound/return tunnel traffic always
      #       exits wg0, preventing asymmetric routing.
      #     • Pod egress SNATs to sub_ipv6 via Flannel, then exits via wg0.
      #
      #   wg1 — node mesh interface (private address sub_ipv6_private = fd00:cafe::...)
      #     • K3s, Flannel VXLAN, kubelet, and local-api fan-out use this.
      #     • A single destination route sends all cluster-subnet traffic here
      #       regardless of source address (needed for VXLAN sockets).
      #
      # Decoupling the two interfaces means tunnel clients (public access only)
      # and mesh nodes (cluster only) can be provisioned independently.
      #
      # Routing rules:
      #
      #  A. Destination route on wg1 (main table):
      #       ip -6 route add <privateSubnet> dev wg1
      #     Cluster-bound traffic exits wg1, regardless of source.
      #
      #  B. Source policy on wg0 (table 51820):
      #       ip -6 rule add from <sub_ipv6> lookup 51820
      #       ip -6 route add ::/0 dev wg0 table 51820
      #     Outbound/return traffic from our public address exits wg0.
      #
      #  C. Default route on wg0 (metric 200):
      #     Pod traffic (fd00:42::/56) SNATs to sub_ipv6, then exits via wg0.
      wireguard.interfaces.wg0 = {
        ips = ["${s.tunnelCfg.sub_ipv6}/128"];
        privateKey = s.tunnelCfg.wg_private_key;

        postSetup = ''
          # B. Source policy: public address always exits wg0.
          ip -6 rule add from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
          ip -6 route replace ::/0 dev wg0 table 51820 2>/dev/null || true

          # C. Default route: pod traffic exits via wg0 for outbound IPv6.
          #    metric 200 loses to any ISP-provided default route and wins only
          #    when no ISP IPv6 exists.
          ip -6 route replace ::/0 dev wg0 metric 200 2>/dev/null || true
        '';

        preShutdown = ''
          ip -6 rule del from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
          ip -6 route del ::/0 dev wg0 table 51820 2>/dev/null || true
          ip -6 route del ::/0 dev wg0 metric 200 2>/dev/null || true
        '';

        peers = [
          {
            publicKey = s.tunnelCfg.wg_server_public_key;
            endpoint = s.tunnelCfg.wg_server_endpoint;
            allowedIPs = ["::/0"];
            persistentKeepalive = 25;
          }
        ];
      };

      wireguard.interfaces.wg1 = {
        ips = ["${s.nodeCfg.sub_ipv6_private}/128"];
        privateKey = s.nodeCfg.wg_private_key;

        postSetup = ''
          # A. Destination route: all cluster-node IPs go through wg1.
          ip -6 route replace ${s.privateSubnet} dev wg1 2>/dev/null || true
        '';

        preShutdown = ''
          ip -6 route del ${s.privateSubnet} dev wg1 2>/dev/null || true
        '';

        peers = [
          {
            publicKey = s.nodeCfg.wg_server_public_key;
            endpoint = s.nodeCfg.wg_server_endpoint;
            allowedIPs = ["${s.privateSubnet}"];
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
    #   wg1 already encrypts all inter-node traffic end-to-end (node mesh key).
    #   wireguard-native would add a second WireGuard layer on top (double
    #   encapsulation, ~2× overhead, more complex routing).  With vxlan, pod
    #   traffic is encapsulated then encrypted once by wg1 — simpler and faster.
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
        # k3s also ships local-path-provisioner and marks its StorageClass as
        # the cluster default. Nothing here uses it — every app goes to
        # yolab-cephfs — and Kubernetes REJECTS PVC creation while two default
        # classes exist, which is what shipping both produced: k3s re-applied
        # local-path (and its default annotation) on every restart, racing
        # whatever tried to strip it.
        "--disable=local-storage"
        "--flannel-backend=vxlan"
        "--flannel-ipv6-masq"
        "--cluster-cidr=fd00:42::/56,10.42.0.0/16"
        "--service-cidr=fd00:43::/112,10.43.0.0/16"
        "--cluster-dns=fd00:43::a"
        "--advertise-address=${s.nodeCfg.sub_ipv6_private}"
        "--tls-san=${s.nodeCfg.sub_ipv6_private}"
        "--resolv-conf=/etc/k3s-resolv.conf"
      ];
    };

    # Detect the node's outbound IPv4 at boot and write it to K3s's config file
    # as node-ip alongside the private IPv6, enabling dual-stack pods.
    # Running before K3s and after WireGuard ensures the IPv6 address is up.
    systemd.services.k3s-node-ip = {
      description = "Write K3s dual-stack node-ip config";
      after = [
        "wireguard-wg1.service"
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
          CONFIG="${config.yolab.repoPath}/homelab/ignored/config.toml"
          mkdir -p /etc/rancher/k3s
          {
            if [ -n "$IPV4" ]; then
              echo "node-ip: ${s.nodeCfg.sub_ipv6_private},$IPV4"
            else
              echo "node-ip: ${s.nodeCfg.sub_ipv6_private}"
            fi
          } > /etc/rancher/k3s/config.yaml
          chmod 600 /etc/rancher/k3s/config.yaml
        '';
      };
    };

    # K3s must start after both WireGuard interfaces so all addresses are up
    # before K3s tries to register itself with the cluster.
    systemd.services.k3s = {
      after = [
        "wireguard-wg0.service"
        "wireguard-wg1.service"
        "k3s-node-ip.service"
      ];
      wants = [
        "wireguard-wg0.service"
        "wireguard-wg1.service"
      ];
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
          # To local-api, not to a fixed address. This used to point at
          # [fd00:43::cefd]:7000 — the ClusterIP of Rook's dashboard Service —
          # which stopped existing when Ceph moved out of Kubernetes, and the
          # link has returned 502 ever since.
          #
          # It cannot point at the local mgr either: the dashboard is served by
          # the ACTIVE mgr, every node runs one, and a standby answers with a
          # redirect to an address on the WireGuard mesh that no browser can
          # reach. local-api asks Ceph which mgr is active and forwards there,
          # so a failover changes nothing here.
          handle /ceph-dashboard* {
            forward_auth [::1]:3001 {
              uri /api/auth/check
            }
            reverse_proxy [::1]:3001
          }
          handle {
            root * ${s.clientUi}
            try_files {path} /index.html
            # Vite gives every asset a content hash in its filename, so those
            # are safe to cache forever — a new build produces new names.
            # index.html is the one file whose name never changes, and it is
            # what points at those hashed names. Cached, it keeps requesting
            # yesterday's bundle, so a deployed fix stays invisible until
            # someone happens to hard-refresh. That wasted a debugging session
            # chasing UI bugs that were already fixed on disk.
            # Two matchers, deliberately disjoint. A bare `header` block would
            # also match the hashed assets and cancel the immutable caching,
            # since Caddy applies every matching header directive.
            @hashed path_regexp \.[0-9a-zA-Z_-]{8,}\.(js|css|woff2?|png|svg|jpg|webp)$
            header @hashed Cache-Control "public, max-age=31536000, immutable"
            @entry path / /index.html
            header @entry Cache-Control "no-cache"
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
    # The system OSD is now a dedicated LVM logical volume (/dev/pool/ceph),
    # created by disko at install time and activated automatically by LVM on
    # every boot — so there is no loop-file service to attach or self-heal, and
    # no ENOSPC coupling between the OSD and the OS root filesystem. Rook consumes
    # /dev/mapper/pool-ceph directly (see disks_reconciler). Additional whole
    # disks are consumed as raw devices, discovered by the reconciler.

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
        YOLAB_NODE_IPV6 = s.nodeCfg.sub_ipv6_private;
        KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
        NIX_SSL_CERT_FILE = "/etc/static/ssl/certs/ca-bundle.crt";
        SSL_CERT_FILE = "/etc/static/ssl/certs/ca-bundle.crt";
      };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "5s";
        ExecStart = "${s.localApiEnv}/bin/local-api";
      };
    };

    # No storage-class default management here any more.
    #
    # There used to be a yolab-storageclass-default unit that stripped the
    # default annotation off k3s's local-path class. It is gone because
    # --disable=local-storage above means that class is never created, and
    # yolab-cephfs (which declares the annotation itself, in
    # rook/cluster-external.yaml) is the only default.
    #
    # Removing it was NOT optional once local-storage was disabled: its
    # ExecStart began `until kubectl get storageclass local-path; do sleep 5;
    # done` — an unbounded wait inside a oneshot unit, which would have hung at
    # every boot waiting for a class that can no longer exist.

    # ── CephFS CSI stale-lock recovery ───────────────────────────────────────
    # After a node reboot the CephFS CSI plugin (csi-cephfsplugin DaemonSet)
    # retains in-memory operation locks from the previous session.  Any pod that
    # tries to mount a CephFS volume immediately after reboot gets:
    #   "an operation with the given Volume ID … already exists"
    # until those locks expire (~10 minutes) or the pod is restarted.
    # Restarting the DaemonSet pods on startup clears the lock state immediately
    # so app pods and VolSync backup jobs can mount volumes without delay.
    systemd.services.yolab-csi-recovery = {
      description = "Restart CephFS CSI plugin to clear stale volume locks";
      after = ["k3s.service"];
      wantedBy = ["multi-user.target"];
      environment.KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "csi-recovery" ''
          export PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH
          # Wait for the CSI DaemonSet to exist — Rook may not have reconciled yet.
          until kubectl get daemonset csi-cephfsplugin -n rook-ceph 2>/dev/null; do sleep 10; done
          # Delete only THIS node's plugin pod. The stale locks being cleared are held in
          # the local plugin's memory from the session before this node rebooted, so a
          # `rollout restart` of the whole DaemonSet — which is what this used to do —
          # bounced the plugin on every other node too, interrupting their live CephFS
          # mounts for a problem they do not have.
          kubectl delete pod -n rook-ceph -l app=csi-cephfsplugin \
            --field-selector "spec.nodeName=$(cat /etc/hostname)" --ignore-not-found || true
        '';
        TimeoutStartSec = "300";
      };
    };

    # ── Users ─────────────────────────────────────────────────────────────
    users.users.root.openssh.authorizedKeys.keys =
      lib.optional (s.rootSshKey != "") s.rootSshKey
      ++ [
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK4KqHP17dqZURgVG7NwJ4sRoPVpmmNb3fMhGiWD529z nixos@nixos"
      ];

    users.users.homelab = {
      isNormalUser = true;
      extraGroups = ["wheel"];
      openssh.authorizedKeys.keys = s.allowedSshKeys;
      hashedPassword = lib.mkIf (s.homelabPasswordHash != "") s.homelabPasswordHash;
    };

    services.logind.settings.Login.HandleLidSwitchExternalPower = "ignore";

    # ── Boot banner ───────────────────────────────────────────────────────────────
    # Generates /run/issue with a QR code and management URL before tty1 shows
    # the login prompt.  agetty is configured to display that file.
    systemd.services.yolab-banner = {
      description = "Generate boot banner with management URL QR code";
      before = ["getty@tty1.service"];
      wantedBy = ["getty@tty1.service"];
      after = ["local-fs.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "yolab-banner" ''
          CONFIG="${config.yolab.repoPath}/homelab/ignored/config.toml"
          DNS_URL=""

          if [ -f "$CONFIG" ]; then
            DNS_URL=$(${pkgs.gnugrep}/bin/grep -oP 'dns_url\s*=\s*"\K[^"]+' "$CONFIG" 2>/dev/null || true)
          fi

          {
            printf '\n'
            if [ -n "$DNS_URL" ]; then
              ${pkgs.qrencode}/bin/qrencode -t UTF8 -m 1 "$DNS_URL" 2>/dev/null || true
              printf '\n  YoLab Management: %s\n\n' "$DNS_URL"
            else
              printf '  YoLab — not yet configured\n\n'
            fi
          } > /run/issue
        '';
      };
    };

    services.getty.extraArgs = ["--issue-file" "/run/issue" "--noclear"];

    environment.systemPackages = with pkgs;
      map lib.lowPrio [
        curl
        gitMinimal
        just
        wireguard-tools
        kubectl
        gptfdisk # sgdisk — wipes disks before Rook claims them
        unzip # local-api unpacks a chart uploaded as a .zip; tar covers .tgz
        dysk
        dust
        ctop
        vim
        wget
        htop
        sshfs
        fuse3
        qrencode
        restic
        kubernetes-helm # apps are Helm charts; local-api shells out to `helm`
      ];

    # ── Ceph ──────────────────────────────────────────────────────────────────

    # The ceph group is no longer declared here. It used to be pinned to gid 167
    # to match the uid/gid *inside* Rook's OSD containers, so udev could grant
    # those pods access by name. Ceph now runs as host daemons, so
    # services.ceph owns the user and group (gid 288) and declaring it here as
    # well is a conflicting definition that fails evaluation outright.

    # Any whole-disk block device (not a partition, loop, or dm volume) that
    # appears or changes gets group ownership set to "ceph" with mode 0660.
    # This fires on every connect/reconnect, so hot-plugged USB drives and
    # newly provisioned disks are readable by the host's ceph-osd daemons
    # without declaring them individually.
    services.udev.extraRules = ''
      SUBSYSTEM=="block", ENV{DEVTYPE}=="disk", KERNEL!="loop*", KERNEL!="dm-*", GROUP="ceph", MODE="0660"
    '';

    # K3s watches /var/lib/rancher/k3s/server/manifests/ and auto-applies
    # any YAML placed there.  Symlinks into the Nix store so updates
    # propagate on nixos-rebuild without manual kubectl apply.
    systemd.tmpfiles.rules = [
      # Kubelet's drop-in config directory. k3s writes its own
      # 00-k3s-defaults.conf here fresh on every start; this coexists with it
      # (higher sort order = applied on top) rather than fighting it — the
      # directory must exist before k3s's first write, hence the `d` rule.
      "d /var/lib/rancher/k3s/agent/etc/kubelet.conf.d 0700 root root -"
      "L+ /var/lib/rancher/k3s/agent/etc/kubelet.conf.d/10-yolab-image-gc.conf     - - - - ${./k3s/kubelet-image-gc.yaml}"
      # Rook's operator still runs — it owns ceph-csi, which is what backs PVCs —
      # but it no longer runs the Ceph cluster itself. The CephCluster/
      # CephFilesystem manifests are deliberately NOT applied here: Ceph is a
      # host daemon now (homelab/nixos/ceph/), because a mon that is a pod makes
      # an RBD-backed containerd store impossible. See that module's header.
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-operator.yaml              - - - - ${./rook/operator.yaml}"
      # External-mode CephCluster + the yolab-cephfs StorageClass. Replaces the
      # old cluster.yaml, which declared mons/mgrs/OSDs that Rook no longer runs.
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-external.yaml              - - - - ${./rook/cluster-external.yaml}"
      # external-snapshotter: CRDs + RBAC must be applied before the controller.
      # K3s applies manifests in lexicographic order so the prefix ensures ordering.
      "L+ /var/lib/rancher/k3s/server/manifests/snap-1-crds-rbac.yaml                - - - - ${./external-snapshotter/crds-rbac.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/snap-2-controller.yaml               - - - - ${./external-snapshotter/controller.yaml}"
      # VolSync operator for PV backup/restore via restic.
      "L+ /var/lib/rancher/k3s/server/manifests/volsync.yaml                         - - - - ${./volsync/helmchart.yaml}"
      # VolumeSnapshotClass for Rook CephFS CSI — used by VolSync ReplicationSources.
      # Applied after VolSync so the CRD (from external-snapshotter) exists first.
      "L+ /var/lib/rancher/k3s/server/manifests/volsync-snapshotclass.yaml           - - - - ${./volsync/snapshotclass.yaml}"
      # BackupRun/RestoreRun CRDs — local-api's backup/restore orchestration state
      # lives in these objects (see homelab/local-api/src/routers/backup_run.rs and
      # restore_run.rs) instead of ConfigMap flags, so a crashed local-api or a stuck
      # step can always be recomputed from status instead of getting stuck forever.
      "L+ /var/lib/rancher/k3s/server/manifests/yolab-crd-backuprun.yaml             - - - - ${./yolab-crds/backuprun-crd.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/yolab-crd-restorerun.yaml            - - - - ${./yolab-crds/restorerun-crd.yaml}"
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
    # Limit build parallelism so deploys don't starve k3s and Ceph.
    # One job at a time, capped at 2 cores — Rust link phase is single-threaded anyway.
    nix.settings.max-jobs = 1;
    nix.settings.cores = 2;
    nix.gc.automatic = true;
    # Automatic was already on, but with no options it only clears truly
    # unreachable garbage — every generation from every past rebuild stays a
    # GC root forever and /nix grows without bound. Bounding it to 2 weeks is
    # part of the same "root disk must not silently fill up" fix as the
    # kubelet image-gc thresholds above.
    nix.gc.options = "--delete-older-than 14d";

    # Swap is allocated on demand rather than reserved up front. The fixed 8 GB swapfile
    # this replaces sat on the root LV permanently, used or not, and root is whatever is
    # left after disko hands 100%FREE to Ceph — so it was 8 GB taken from the smaller of
    # the two volumes to cover a case that mostly never happens.
    #
    # With vm.swappiness = 10 the kernel avoids swapping anyway, so in the common case
    # swapspace allocates nothing at all.
    swapDevices = [];
    services.swapspace = {
      enable = true;
      settings = {
        # Per-file cap. The module default is "2t", which on a laptop is not a limit.
        max_swapsize = "4g";
        # swapspace stops when the disk fills and then backs off for `cooldown` seconds,
        # but total swap is otherwise bounded only by free space — and this LV also holds
        # /nix. A full root here does not just mean swap thrashing, it means
        # nixos-rebuild can no longer write, i.e. the node loses the ability to update or
        # roll back. Keeping a larger free margin than the default (20/60/30) is cheap
        # insurance against the one failure this product cannot recover from remotely.
        lower_freelimit = 15;
        freetarget = 25;
      };
    };

    # Reclaim the old fixed swapfile from nodes that were built before the switch —
    # guarded, because deleting a file the kernel is actively swapping to would take the
    # machine down. After a reboot it is no longer in /proc/swaps and can go.
    systemd.services.yolab-drop-legacy-swapfile = {
      description = "Remove the pre-swapspace fixed swapfile once it is inactive";
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "drop-legacy-swapfile" ''
          if [ -f /var/lib/swapfile ] && ! ${pkgs.gnugrep}/bin/grep -q '/var/lib/swapfile' /proc/swaps; then
            rm -f /var/lib/swapfile && echo "removed legacy /var/lib/swapfile"
          fi
        '';
      };
    };
  };
}
