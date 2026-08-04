{
  pkgs,
  lib,
  config,
  inputs,
  ...
}:
let
  s = import ../shared.nix { inherit pkgs lib inputs; };
  k3sCfg = s.nodeCfg.k3s;

  # The first node initialises the embedded-etcd cluster (--cluster-init).
  # Every other node joins as an equal server peer via serverAddr.
  # After joining, all nodes are identical: control plane + worker + UI.
  isFirstNode = k3sCfg.server_addr == "";

  tunnelDomain = lib.removePrefix "https://" (lib.removePrefix "http://" s.tunnelCfg.dns_url);
in
{
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
    # glances 4.5.5: REST tests race a locally-started server (connection
    # refused). Remove those two files in postPatch so pytest never sees them.
    nixpkgs.overlays = [
      (_final: prev: {
        glances = prev.glances.overrideAttrs (old: {
          postPatch = (old.postPatch or "") + ''
            rm -f tests/test_restful.py tests/test_browser_restful.py
          '';
        });
      })
    ];

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
        ips = [ "${s.tunnelCfg.sub_ipv6}/128" ];
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
            allowedIPs = [ "::/0" ];
            persistentKeepalive = 25;
          }
        ];
      };

      wireguard.interfaces.wg1 = {
        ips = [ "${s.nodeCfg.sub_ipv6_private}/128" ];
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
            allowedIPs = [ "${s.privateSubnet}" ];
            persistentKeepalive = 25;
          }
        ];
      };
    };

    # ── SSH ───────────────────────────────────────────────────────────────
    services.openssh = {
      enable = true;
      ports = [ s.sshPort ];
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
      wants = [ "network-online.target" ];
      before = [ "k3s.service" ];
      wantedBy = [ "k3s.service" ];
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
          @glances_exact path /glances
          redir @glances_exact /glances/ 301
          handle /glances/* {
            forward_auth [::1]:3001 {
              uri /api/auth/check
            }
            uri strip_prefix /glances
            reverse_proxy 127.0.0.1:61208
          }
          handle /ceph-dashboard/* {
            forward_auth [::1]:3001 {
              uri /api/auth/check
            }
            reverse_proxy [fd00:43::cefd]:7000
          }
          handle {
            root * ${s.clientUi}
            try_files {path} /index.html
            file_server
          }
        }
      '';
    };

    systemd.services.glances = {
      description = "Glances system monitor";
      wantedBy = [ "multi-user.target" ];
      after = [ "network.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${pkgs.glances}/bin/glances -w --port 61208 --disable-plugin docker";
        Restart = "on-failure";
        RestartSec = "5s";
      };
    };

    systemd.services.caddy = {
      after = [ "wireguard-wg0.service" ];
      wants = [ "wireguard-wg0.service" ];
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
      wants = [ "k3s.service" ];
      wantedBy = [ "multi-user.target" ];
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

    # ── osd-node-controller legacy cleanup ───────────────────────────────────
    # Disk reconciliation moved into yolab-local-api (Rust). Delete the old
    # Python DaemonSet and its RBAC once the cluster is reachable.
    systemd.services.yolab-osd-controller-cleanup = {
      description = "Remove legacy osd-node-controller DaemonSet";
      after = [ "k3s.service" ];
      wantedBy = [ "multi-user.target" ];
      environment.KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "osd-controller-cleanup" ''
          export PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH
          until kubectl get nodes 2>/dev/null; do sleep 5; done
          # Remove K3s Addon objects so K3s stops managing these resources.
          kubectl delete addon rook-osd-controller    -n kube-system --ignore-not-found 2>/dev/null || true
          kubectl delete addon rook-osd-controller-cm -n kube-system --ignore-not-found 2>/dev/null || true
          # Remove the K8s resources themselves.
          kubectl delete daemonset    osd-node-controller        -n rook-ceph --ignore-not-found
          kubectl delete serviceaccount osd-node-controller      -n rook-ceph --ignore-not-found
          kubectl delete role         osd-node-controller        -n rook-ceph --ignore-not-found
          kubectl delete rolebinding  osd-node-controller        -n rook-ceph --ignore-not-found
          kubectl delete configmap    osd-node-controller-script -n rook-ceph --ignore-not-found
        '';
      };
    };

    # ── Storage class default management ─────────────────────────────────────
    # K3s creates a `local-path` StorageClass on every boot and marks it as
    # the cluster default.  We deploy `yolab-cephfs` as the real default (set
    # explicitly after K3s is ready) so that all PVC requests go to CephFS.
    # Without this, both classes would be marked default — K8s 1.26+ rejects
    # PVC creation when more than one default class exists.
    systemd.services.yolab-storageclass-default = {
      description = "Ensure yolab-cephfs is the only default StorageClass";
      after = [ "k3s.service" ];
      wantedBy = [ "multi-user.target" ];
      environment.KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # Retry until the API server is ready (K3s may still be initialising).
        ExecStart = pkgs.writeShellScript "storageclass-default" ''
          export PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH
          until kubectl get storageclass local-path 2>/dev/null; do sleep 5; done
          kubectl annotate storageclass local-path \
            storageclass.kubernetes.io/is-default-class=false --overwrite
          kubectl annotate storageclass yolab-cephfs \
            storageclass.kubernetes.io/is-default-class=true --overwrite 2>/dev/null || true
        '';
      };
    };

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
      after = [ "k3s.service" ];
      wantedBy = [ "multi-user.target" ];
      environment.KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "csi-recovery" ''
          export PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH
          # Wait for the CSI DaemonSet to exist — Rook may not have reconciled yet.
          until kubectl get daemonset csi-cephfsplugin -n rook-ceph 2>/dev/null; do sleep 10; done
          kubectl rollout restart daemonset/csi-cephfsplugin -n rook-ceph
          kubectl rollout status  daemonset/csi-cephfsplugin -n rook-ceph --timeout=180s || true
        '';
        TimeoutStartSec = "300";
      };
    };

    # ── Users ─────────────────────────────────────────────────────────────
    users.users.root.openssh.authorizedKeys.keys = lib.optional (s.rootSshKey != "") s.rootSshKey ++ [
      "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIK4KqHP17dqZURgVG7NwJ4sRoPVpmmNb3fMhGiWD529z nixos@nixos"
    ];

    users.users.homelab = {
      isNormalUser = true;
      extraGroups = [ "wheel" ];
      openssh.authorizedKeys.keys = s.allowedSshKeys;
      hashedPassword = lib.mkIf (s.homelabPasswordHash != "") s.homelabPasswordHash;
    };

    services.logind.settings.Login.HandleLidSwitchExternalPower = "ignore";

    # ── Boot banner ───────────────────────────────────────────────────────────────
    # Generates /run/issue with a QR code and management URL before tty1 shows
    # the login prompt.  agetty is configured to display that file.
    systemd.services.yolab-banner = {
      description = "Generate boot banner with management URL QR code";
      before = [ "getty@tty1.service" ];
      wantedBy = [ "getty@tty1.service" ];
      after = [ "local-fs.target" ];
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

    services.getty.extraArgs = [ "--issue-file" "/run/issue" "--noclear" ];

    environment.systemPackages =
      with pkgs;
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
        glances
        sshfs
        fuse3
        qrencode
        restic
        kubernetes-helm # apps are Helm charts; local-api shells out to `helm`
      ];

    # ── Helm chart dependencies ───────────────────────────────────────────────
    # Every app chart depends on the yolab-common library chart (the tunnel gateway).
    # Helm resolves `file://../yolab-common` into charts/*.tgz, which is a build
    # artifact rather than source, so it is gitignored and materialised here instead —
    # otherwise `git reset --hard` during an update would delete it and every install
    # would fail with "found in Chart.yaml, but missing in charts/ directory".
    #
    # Runs before local-api so a freshly-updated node can never serve an install from a
    # chart whose dependency hasn't been vendored yet.
    systemd.services.yolab-chart-deps = {
      description = "Vendor Helm chart dependencies for the app catalog";
      before = [ "yolab-local-api.service" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = pkgs.writeShellScript "yolab-chart-deps" ''
          export PATH=/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:$PATH
          CATALOG="${config.yolab.repoPath}/apps/catalog"
          [ -d "$CATALOG" ] || exit 0
          for chart in "$CATALOG"/*/; do
            [ -f "$chart/Chart.yaml" ] || continue
            grep -q '^dependencies:' "$chart/Chart.yaml" || continue
            # `dependency update` rather than `build`: Chart.lock is gitignored too, so
            # there is not always a lock to build from.
            ${pkgs.kubernetes-helm}/bin/helm dependency update "$chart" >/dev/null 2>&1 \
              || echo "warning: could not vendor deps for $chart" >&2
          done
        '';
      };
    };

    # ── Rook / Ceph ───────────────────────────────────────────────────────────

    # Ceph OSD containers run as uid/gid 167 (ceph:ceph) inside the pod.
    # Declare the group on the host so udev can reference it by name.
    users.groups.ceph = { gid = 167; };

    # Any whole-disk block device (not a partition, loop, or dm volume) that
    # appears or changes gets group ownership set to "ceph" with mode 0660.
    # This fires on every connect/reconnect, so hot-plugged USB drives and
    # newly provisioned disks are automatically accessible to Rook OSD pods
    # without declaring them individually.
    services.udev.extraRules = ''
      SUBSYSTEM=="block", ENV{DEVTYPE}=="disk", KERNEL!="loop*", KERNEL!="dm-*", GROUP="ceph", MODE="0660"
    '';

    # K3s watches /var/lib/rancher/k3s/server/manifests/ and auto-applies
    # any YAML placed there.  Symlinks into the Nix store so updates
    # propagate on nixos-rebuild without manual kubectl apply.
    systemd.tmpfiles.rules = [
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-operator.yaml              - - - - ${./rook/operator.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-cluster.yaml               - - - - ${./rook/cluster.yaml}"
      # Applies norebalance OSD flag after the cluster is ready. restartPolicy:OnFailure
      # retries until Ceph is up; ttlSecondsAfterFinished cleans it up after success.
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-cluster-init.yaml          - - - - ${./rook/cluster-init-job.yaml}"
      # Remove legacy osd-node-controller manifest files so K3s stops recreating the
      # DaemonSet. The r directive silently no-ops if the file is already gone.
      "r /var/lib/rancher/k3s/server/manifests/rook-osd-controller.yaml"
      "r /var/lib/rancher/k3s/server/manifests/rook-osd-controller-cm.yaml"
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

    swapDevices = [
      {
        device = "/var/lib/swapfile";
        size = 8192;
      }
    ];
    services.swapspace.enable = true;
  };
}
