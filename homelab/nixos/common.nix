{
  pkgs,
  lib,
  config,
  yolabConfigPath,
  rust,
  localApiEnv,
  yolabRev,
  yolabLastModified,
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

  ipv6 = import ./ipv6.nix {inherit lib;};
  yolabRange =
    if (s.tunnelCfg.sub_ipv6 or "") == "" || lib.hasPrefix "[" (s.tunnelCfg.wg_server_endpoint or "")
    then null
    else ipv6.prefix64 s.tunnelCfg.sub_ipv6;

  cephCfg = s.homelabConfig.ceph or {};

  nameservers = [
    "2606:4700:4700::1111"
    "2001:4860:4860::8888"
  ];

  localApi = "[::1]:3001";

  flannelVxlanPort = 8472;
  lanSet4 = "yolab-lan4";
  lanSet6 = "yolab-lan6";

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
    ./gpu
    ./hardware
  ];

  options.yolab = {
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
      fsid = cephCfg.fsid or (throw "[ceph] fsid is required in config.toml");
      monAddr = s.nodeCfg.sub_ipv6_private;
      clusterSubnet = s.privateSubnet;
      joinSeedAddr = cephSeedAddr;
    };

    time.timeZone = s.timezone;
    i18n.defaultLocale = s.locale;

    networking.nameservers = nameservers;

    environment.etc."k3s-resolv.conf".text =
      lib.concatMapStrings (n: "nameserver ${n}\n") (["1.1.1.1" "8.8.8.8"] ++ nameservers);

    networking = {
      hostName = s.hostname;
      enableIPv6 = true;
      firewall = {
        enable = true;
        allowedTCPPorts = [
          80
          443
          s.sshPort
        ];
        allowedUDPPorts = [51821];
        trustedInterfaces = [
          "wg1"
          "cni0"
          "flannel.1"
          "flannel-v6.1"
        ];
        checkReversePath = false;
        extraPackages = [pkgs.ipset];
        extraCommands = ''
          ipset create ${lanSet4} hash:net family inet -exist
          ipset create ${lanSet6} hash:net family inet6 -exist
          iptables -A nixos-fw -p udp --dport ${toString flannelVxlanPort} -m set --match-set ${lanSet4} src -j nixos-fw-accept
          ip6tables -A nixos-fw -p udp --dport ${toString flannelVxlanPort} -m set --match-set ${lanSet6} src -j nixos-fw-accept
        '';
      };

      wireguard.interfaces.wg0 = {
        ips = ["${s.tunnelCfg.sub_ipv6}/128"];
        privateKey = s.tunnelCfg.wg_private_key;
        allowedIPsAsRoutes = false;

        postSetup = ''
          if [ -z "$(ip -6 rule show from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100)" ]; then
            ip -6 rule add from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100
          fi
          ip -6 route replace ::/0 dev wg0 table 51820
          ${lib.optionalString (
            yolabRange != null
          ) "ip -6 route replace ${yolabRange} dev wg0"}
        '';

        preShutdown = ''
          if [ -n "$(ip -6 rule show from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100)" ]; then
            ip -6 rule del from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100
          fi
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
          ip -6 route replace ${s.privateSubnet} dev wg1
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

    boot.kernelPackages = pkgs.linuxPackages_latest;

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
        "--disable=metrics-server"
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

    systemd.services.yolab-lan-subnets = {
      description = "Keep the firewall's LAN subnets (allowed to send flannel VXLAN) current";
      after = ["firewall.service"];
      wantedBy = ["multi-user.target"];
      startLimitIntervalSec = 0;
      serviceConfig = {
        Restart = "always";
        RestartSec = "5";
        ExecStart = "${localApiEnv}/bin/local-api boot lan-subnets";
      };
      path = [
        pkgs.iproute2
        pkgs.ipset
      ];
      environment = {
        YOLAB_LAN_SET4 = lanSet4;
        YOLAB_LAN_SET6 = lanSet6;
      };
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
        hash = "sha256-hlu5ryAU9eUGVQmDrAF6tWKQ6wbJTwkVg8crx4497kU=";
      };
      configFile = pkgs.writeText "Caddyfile" ''
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

        (yolab_ui) {
          handle /api/* {
            reverse_proxy ${localApi}
          }
          handle ${config.yolab.ceph.dashboard.urlPrefix}* {
            forward_auth ${localApi} {
              uri /api/auth/check
            }
            reverse_proxy ${localApi}
          }
          handle {
            root * ${s.clientUi}
            try_files {path} /index.html
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

        cluster.${userDomain} {
          import shared_tls cluster
          import yolab_ui
        }

        notify.${userDomain} {
          import shared_tls notify
          reverse_proxy [::1]:2586 {
            flush_interval -1
          }
        }
      '';
    };

    systemd.services.caddy = {
      after = [
        "wireguard-wg0.service"
        "yolab-caddy-credentials.service"
      ];
      wants = [
        "wireguard-wg0.service"
        "yolab-caddy-credentials.service"
      ];
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
        ExecStart = "${localApiEnv}/bin/local-api shared-names credentials";
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
        ExecStart = "${localApiEnv}/bin/local-api notify credentials";
      };
    };

    systemd.services.yolab-local-api = {
      after = ["network.target"];
      wants = ["k3s.service"];
      wantedBy = ["multi-user.target"];
      environment = {
        PATH = lib.mkForce (
          lib.makeBinPath (with pkgs; [ceph ceph-client lvm2 util-linux xfsprogs e2fsprogs coreutils systemd])
          + ":/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin"
        );
        YOLAB_MACHINE_DIR = config.yolab.machineDir;
        YOLAB_CONFIG = "${config.yolab.machineDir}/config.toml";
        YOLAB_NODE_IPV6 = s.nodeCfg.sub_ipv6_private;
        KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
        NIX_SSL_CERT_FILE = "/etc/static/ssl/certs/ca-bundle.crt";
        SSL_CERT_FILE = "/etc/static/ssl/certs/ca-bundle.crt";
        YOLAB_LIBRADOS = "${pkgs.ceph.lib}/lib/librados.so.2";
        YOLAB_CEPH_FSID = config.yolab.ceph.fsid;
        YOLAB_CEPH_MON_ADDR = config.yolab.ceph.monAddr;
        YOLAB_CEPH_JOIN_SEED_ADDR = config.yolab.ceph.joinSeedAddr;
        YOLAB_CEPH_IMAGES_POOL = config.yolab.ceph.imagesStore.poolName;
        YOLAB_CEPH_IMAGES_SHARE = toString config.yolab.ceph.imagesStore.shareOfPool;
        YOLAB_CEPH_IMAGES_MIN_GB = toString config.yolab.ceph.imagesStore.minSizeGb;
        YOLAB_CEPH_IMAGES_FS = config.yolab.ceph.imagesStore.filesystem;
        YOLAB_CEPH_CSI_MANIFESTS = "${./ceph-csi/cephfs.yaml}";
        YOLAB_CEPH_DASHBOARD_PORT = toString config.yolab.ceph.dashboard.port;
        YOLAB_CEPH_DASHBOARD_PREFIX = config.yolab.ceph.dashboard.urlPrefix;
        YOLAB_CEPH_DASHBOARD_PASSWORD_FILE = config.yolab.ceph.dashboard.passwordFile;
      };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "5s";
        ExecStart = "${localApiEnv}/bin/local-api";
      };
    };

    systemd.services.yolab-reset-wipe = let
      host = config.networking.hostName;
    in {
      description = "Wipe this machine's cluster state for a FORCE HEAL";
      wantedBy = ["multi-user.target"];
      after = [
        "local-fs.target"
        "systemd-tmpfiles-setup.service"
      ];
      before = [
        "yolab-ceph-bootstrap.service"
        "ceph-mon-${host}.service"
        "ceph-mgr-${host}.service"
        "ceph-mds-${host}.service"
        "k3s-node-ip.service"
        "k3s.service"
        "yolab-local-api.service"
      ];
      requiredBy = [
        "yolab-ceph-bootstrap.service"
        "k3s.service"
      ];
      restartIfChanged = false;
      path = with pkgs; [
        ceph
        lvm2
        util-linux
        coreutils
      ];
      environment.YOLAB_MACHINE_DIR = config.yolab.machineDir;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "1800s";
        ExecStart = "${localApiEnv}/bin/local-api storage reset-wipe";
      };
    };

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
      "L+ /var/lib/rancher/k3s/server/manifests/snap-1-crds-rbac.yaml                - - - - ${./external-snapshotter/crds-rbac.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/snap-2-controller.yaml               - - - - ${./external-snapshotter/controller.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/volsync.yaml                         - - - - ${./volsync/helmchart.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/volsync-snapshotclass.yaml           - - - - ${./volsync/snapshotclass.yaml}"
      "L+ /var/lib/rancher/k3s/server/manifests/gpu-device-plugins.yaml               - - - - ${./gpu/device-plugins.yaml}"
    ];

    system.activationScripts.yolabVersion = ''
      mkdir -p /var/lib/yolab
      printf '%s' ${lib.escapeShellArg yolabRev} > /var/lib/yolab/built-hash
      ${pkgs.coreutils}/bin/date -u -d @${toString yolabLastModified} +%Y-%m-%dT%H:%M:%SZ > /var/lib/yolab/built-date
    '';

    nix.settings.experimental-features = [
      "nix-command"
      "flakes"
    ];
    nix.settings.max-jobs = 1;
    nix.settings.cores = 2;
    nix.settings.fallback = true;

    nix.settings.substituters = [
      "https://cache.nixos.org"
      "https://cache.yolab.io/yolab"
    ];
    nix.settings.trusted-public-keys = [
      "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
      "yolab:LrcqUCJSfUGqbrMyJ8qS8WJdOuDTRlOGOhkhoRALAH8="
    ];
    nix.gc.automatic = true;
    nix.gc.options = "--delete-older-than 14d";

    services.swapspace = {
      enable = true;
    };
  };
}
