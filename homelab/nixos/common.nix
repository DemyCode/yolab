{
  pkgs,
  lib,
  config,
  yolabConfigPath,
  rust,
  localApiEnv,
  yolabRev ? "",
  yolabLastModified ? null,
  ...
}: let
  s = import ../shared.nix {
    inherit
      pkgs
      lib
      yolabConfigPath
      rust
      ;
  };
  k3sCfg = s.nodeCfg.k3s;

  isFirstNode = k3sCfg.server_addr == "";

  tunnelDomain = lib.removePrefix "https://" (lib.removePrefix "http://" s.tunnelCfg.dns_url);
  userDomain = lib.concatStringsSep "." (lib.drop 1 (lib.splitString "." tunnelDomain));
  platformApiUrl = lib.removeSuffix "/" (s.tunnelCfg.platform_api_url or "https://api.yolab.io");

  cephCfg = s.homelabConfig.ceph or {};

  cephSeedAddr =
    if isFirstNode
    then ""
    else let
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
    ./ceph/maintenance.nix
    ./ceph/dashboard.nix
  ];

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
    machineDir = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/yolab/machine";
      description = ''
        Absolute path to this machine's own files (config.toml,
        hardware-configuration.nix) — the directory every rebuild passes as the
        `yolab-machine` flake input. Kept outside the repo; see flake.nix.
      '';
    };
  };

  config = {
    nixpkgs.overlays = [
      (_: prev: {
        python312 = prev.python312.override {
          packageOverrides = _: pyprev: {
            "inline-snapshot" = pyprev."inline-snapshot".overridePythonAttrs (_: {
              doCheck = false;
            });
          };
        };
      })
    ];

    yolab.ceph = {
      enable = true;
      fsid = cephCfg.fsid or (throw "[ceph] fsid is required in config.toml");
      monAddr = s.nodeCfg.sub_ipv6_private;
      clusterSubnet = s.privateSubnet;
      joinSeedAddr = cephSeedAddr;
      imagesStore.enable = true;
      filesystem.enable = true;
      maintenance.enable = true;
    };

    time.timeZone = s.timezone;
    i18n.defaultLocale = s.locale;

    networking.nameservers = [
      "2606:4700:4700::1111"
      "2001:4860:4860::8888"
    ];

    environment.etc."k3s-resolv.conf".text = ''
      nameserver 1.1.1.1
      nameserver 8.8.8.8
      nameserver 2606:4700:4700::1111
      nameserver 2001:4860:4860::8888
    '';

    networking = {
      hostName = s.hostname;
      enableIPv6 = true;
      firewall.enable = false;

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

        listenPort = 51821;

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

    services.openssh = {
      enable = true;
      ports = [s.sshPort];
      settings = {
        PermitRootLogin = "prohibit-password";
        PasswordAuthentication = false;
      };
    };

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
      "vm.swappiness" = 10;
      "vm.dirty_ratio" = 40;
      "vm.dirty_background_ratio" = 10;
    };

    services.k3s = {
      enable = true;
      role = "server";
      inherit (k3sCfg) token;
      clusterInit = isFirstNode;
      serverAddr = k3sCfg.server_addr;

      extraFlags = [
        "--disable=traefik"
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
        ExecStart = "${localApiEnv}/bin/local-api boot node-ip";
      };
      path = [pkgs.iproute2];
      environment.YOLAB_NODE_IPV6 = s.nodeCfg.sub_ipv6_private;
    };

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

      serviceConfig.TimeoutStartSec = lib.mkForce "1800";
    };

    services.caddy = {
      enable = true;
      package = pkgs.caddy.withPlugins {
        plugins = ["github.com/caddy-dns/acmedns@v0.7.0"];
        hash = "sha256-iKExEW87Jd6DXrNBxqvkWkKjkh3KwpNZsBIf1HmSGE4=";
      };
      configFile = pkgs.writeText "Caddyfile" ''
        # A certificate for a shared name: `import shared_tls <name>`. The key is
        # the account token, from the environment file yolab-caddy-credentials
        # writes — never in the Nix store.
        (shared_tls) {
          tls {
            dns acmedns {
              username yolab
              password {env.YOLAB_ACCOUNT_TOKEN}
              subdomain {args[0]}
              server_url ${platformApiUrl}/acme-dns
            }
          }
        }

        # The interface, served identically under this machine's own name and
        # under the name every machine shares.
        (yolab_ui) {
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

        ${tunnelDomain} {
          import yolab_ui
        }

        # Whichever machine is up: the platform's DNS answers with every machine
        # whose tunnel is (see homelab/local-api/src/shared_names.rs).
        cluster.${userDomain} {
          import shared_tls cluster
          import yolab_ui
        }

        # Phone notifications (ntfy, below), from whichever machine is up. A site
        # of its own: ntfy only serves from the root of a host. Subscriptions are
        # long-lived streams, so every event is passed straight through.
        notify.${userDomain} {
          import shared_tls notify
          reverse_proxy [::1]:2586 {
            flush_interval -1
          }
        }
      '';
    };

    systemd.services.caddy = {
      after = ["wireguard-wg0.service" "yolab-caddy-credentials.service"];
      wants = ["wireguard-wg0.service" "yolab-caddy-credentials.service"];
      serviceConfig.EnvironmentFile = "-/var/lib/yolab/caddy/acme.env";
    };

    systemd.services.yolab-caddy-credentials = {
      description = "Give Caddy the key for certificates of the shared names";
      wantedBy = ["multi-user.target"];
      before = ["caddy.service"];
      after = ["local-fs.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "60s";
        ExecStart = "${s.localApiEnv}/bin/local-api shared-names credentials";
      };
    };

    services.ntfy-sh = {
      enable = true;
      settings = {
        base-url = "https://notify.${userDomain}";
        listen-http = "[::1]:2586";
        behind-proxy = true;
        auth-default-access = "deny-all";
        upstream-base-url = "https://ntfy.sh";
      };
      environmentFile = "/var/lib/yolab/ntfy/ntfy.env";
    };

    systemd.services.yolab-ntfy-credentials = {
      description = "Open the cluster's notification topic in ntfy";
      wantedBy = ["multi-user.target"];
      before = ["ntfy-sh.service"];
      requiredBy = ["ntfy-sh.service"];
      after = ["local-fs.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "60s";
        ExecStart = "${s.localApiEnv}/bin/local-api notify credentials";
      };
    };

    systemd.services.yolab-local-api = {
      after = ["network.target"];
      wants = ["k3s.service"];
      wantedBy = ["multi-user.target"];
      environment =
        {
          PATH = lib.mkForce (
            lib.optionalString config.yolab.ceph.enable "${
              lib.makeBinPath (
                with pkgs; [
                  ceph
                  ceph-client
                  lvm2
                  util-linux
                  xfsprogs
                  e2fsprogs
                  coreutils
                  systemd
                ]
              )
            }:"
            + "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin"
          );
          YOLAB_MACHINE_DIR = config.yolab.machineDir;
          YOLAB_CONFIG = "${config.yolab.machineDir}/config.toml";
          YOLAB_PLATFORM = config.yolab.platform;
          YOLAB_FLAKE_TARGET = config.yolab.flakeTarget;
          YOLAB_NODE_IPV6 = s.nodeCfg.sub_ipv6_private;
          KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
          NIX_SSL_CERT_FILE = "/etc/static/ssl/certs/ca-bundle.crt";
          SSL_CERT_FILE = "/etc/static/ssl/certs/ca-bundle.crt";
        }
        // lib.optionalAttrs config.yolab.ceph.enable {
          YOLAB_CEPH_FSID = config.yolab.ceph.fsid;
          YOLAB_CEPH_MON_ADDR = config.yolab.ceph.monAddr;
          YOLAB_CEPH_JOIN_SEED_ADDR = config.yolab.ceph.joinSeedAddr;
          YOLAB_CEPH_IMAGES_POOL = config.yolab.ceph.imagesStore.poolName;
          YOLAB_CEPH_IMAGES_SHARE = toString config.yolab.ceph.imagesStore.shareOfPool;
          YOLAB_CEPH_IMAGES_MIN_GB = toString config.yolab.ceph.imagesStore.minSizeGb;
          YOLAB_CEPH_IMAGES_FS = config.yolab.ceph.imagesStore.filesystem;
          YOLAB_CEPH_DASHBOARD_PORT = toString config.yolab.ceph.dashboard.port;
          YOLAB_CEPH_DASHBOARD_PREFIX = config.yolab.ceph.dashboard.urlPrefix;
          YOLAB_CEPH_DASHBOARD_PASSWORD_FILE = config.yolab.ceph.dashboard.passwordFile;
          YOLAB_CEPH_MDS =
            if config.yolab.ceph.filesystem.enable
            then "1"
            else "0";
        };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "5s";
        ExecStart = "${s.localApiEnv}/bin/local-api";
      };
    };

    systemd.services.yolab-reset-wipe = lib.mkIf config.yolab.ceph.enable (let
      host = config.networking.hostName;
    in {
      description = "Wipe this machine's cluster state for a FORCE HEAL";
      wantedBy = ["multi-user.target"];
      after = ["local-fs.target" "systemd-tmpfiles-setup.service"];
      before = [
        "yolab-ceph-bootstrap.service"
        "ceph-mon-${host}.service"
        "ceph-mgr-${host}.service"
        "ceph-mds-${host}.service"
        "yolab-ceph-system-osd.service"
        "yolab-ceph-osd-activate.service"
        "yolab-images-rbd.service"
        "yolab-containerd-store.service"
        "k3s-node-ip.service"
        "k3s.service"
        "yolab-local-api.service"
      ];
      requiredBy = [
        "yolab-ceph-bootstrap.service"
        "k3s.service"
      ];
      restartIfChanged = false;
      path = with pkgs; [ceph lvm2 util-linux coreutils];
      environment.YOLAB_MACHINE_DIR = config.yolab.machineDir;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "1800s";
        ExecStart = "${s.localApiEnv}/bin/local-api storage reset-wipe";
      };
    });

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

    systemd.services.yolab-banner = {
      description = "Generate boot banner with management URL QR code";
      before = ["getty@tty1.service"];
      wantedBy = ["getty@tty1.service"];
      after = ["local-fs.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        ExecStart = "${localApiEnv}/bin/local-api boot banner";
      };
      path = [pkgs.qrencode];
      environment.YOLAB_CONFIG = "${config.yolab.machineDir}/config.toml";
    };

    services.getty.extraArgs = [
      "--issue-file"
      "/run/issue"
      "--noclear"
    ];

    environment.systemPackages = with pkgs;
      map lib.lowPrio [
        curl
        gitMinimal
        just
        wireguard-tools
        kubectl
        gptfdisk
        unzip
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
        kubernetes-helm
      ];

    services.udev.extraRules = ''
      SUBSYSTEM=="block", ENV{DEVTYPE}=="disk", KERNEL!="loop*", KERNEL!="dm-*", GROUP="ceph", MODE="0660"
    '';

    systemd.tmpfiles.rules = [
      "d ${config.yolab.machineDir} 0700 root root -"
      "d /var/lib/rancher/k3s/agent/etc/kubelet.conf.d 0700 root root -"
      "L+ /var/lib/rancher/k3s/agent/etc/kubelet.conf.d/10-yolab-image-gc.conf     - - - - ${./k3s/kubelet-image-gc.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-operator.yaml              - - - - ${./rook/operator.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/rook-ceph-external.yaml              - - - - ${./rook/cluster-external.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/snap-1-crds-rbac.yaml                - - - - ${./external-snapshotter/crds-rbac.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/snap-2-controller.yaml               - - - - ${./external-snapshotter/controller.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/volsync.yaml                         - - - - ${./volsync/helmchart.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/volsync-snapshotclass.yaml           - - - - ${./volsync/snapshotclass.yaml}"
    ];

    system.activationScripts.yolabVersion = ''
      mkdir -p /var/lib/yolab
      printf '%s' ${lib.escapeShellArg yolabRev} > /var/lib/yolab/built-hash
      ${
        lib.optionalString (yolabLastModified != null) ''
          ${pkgs.coreutils}/bin/date -u -d @${toString yolabLastModified} +%Y-%m-%dT%H:%M:%SZ > /var/lib/yolab/built-date
        ''
      }
      : > /var/lib/yolab/built-message
    '';

    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    nix.settings.max-jobs = 1;
    nix.settings.cores = 2;

    nix.settings.substituters = [
      "https://cache.nixos.org"
      "https://cache.yolab.io/yolab"
    ];
    nix.settings.trusted-public-keys = [
      "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
      "yolab:3CIkfuGsBgTSWSAZJ2FCbVXjLG1RwNJvvGS1MAtQCmQ="
    ];
    nix.gc.automatic = true;
    nix.gc.options = "--delete-older-than 14d";

    services.swapspace = {
      enable = true;
    };
  };
}
