{
  config,
  pkgs,
  lib,
  inputs,
  ...
}:
let
  s = import ../shared.nix { inherit pkgs lib inputs; };
  k3sCfg = s.nodeCfg.k3s or { };
in
{
  options.yolab = {
    platform = lib.mkOption {
      type = lib.types.str;
      default = "nixos";
    };
    flakeTarget = lib.mkOption {
      type = lib.types.str;
      default = "yolab";
    };
    repoPath = lib.mkOption {
      type = lib.types.str;
      default = "/etc/nixos";
    };
  };

  config = {
    boot.initrd.secrets = {
      "/keyfile.bin" = "/etc/secrets/initrd/keyfile.bin";
    };

    boot.initrd.luks.devices."crypted" = {
      keyFile = "/keyfile.bin";
      preLVM = true;
      allowDiscards = true;
    };

    time.timeZone = s.timezone;
    i18n.defaultLocale = s.locale;

    networking = {
      hostName = s.hostname;
      enableIPv6 = true;
      firewall.enable = false;

      wireguard.interfaces = lib.mkIf s.tunnelEnabled {
        wg0 = {
          ips = [ "${s.tunnelCfg.sub_ipv6}/128" ];
          privateKey = s.tunnelCfg.wg_private_key;
          peers = [
            {
              publicKey = s.tunnelCfg.wg_server_public_key;
              endpoint = s.tunnelCfg.wg_server_endpoint;
              allowedIPs = [ s.wgSubnet ];
              persistentKeepalive = 25;
            }
          ];
        };
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

    services.k3s = lib.mkIf s.swarmEnabled {
      enable = true;
      role = k3sCfg.role or "server";
      token = k3sCfg.token or "";
      serverAddr = lib.optionalString (k3sCfg.role or "server" == "agent") (k3sCfg.server_addr or "");
      extraFlags = lib.concatStringsSep " " (
        [
          "--flannel-backend=wireguard-native"
        ]
        ++ lib.optionals s.tunnelEnabled [
          "--advertise-address=${s.tunnelCfg.sub_ipv6}"
          "--node-ip=${s.tunnelCfg.sub_ipv6}"
        ]
        ++ lib.optionals (k3sCfg.role or "server" == "server" && (k3sCfg.server_addr or "") == "") [
          "--cluster-init"
        ]
      );
    };

    systemd.tmpfiles.rules = lib.mkIf s.swarmEnabled [
      "d /var/lib/rancher/k3s/server/manifests 0755 root root -"
      "L /var/lib/rancher/k3s/server/manifests/yolab-csi.yaml - - - - ${./k3s-manifests/yolab-csi.yaml}"
    ];

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
        ++ lib.optionals s.tunnelEnabled [
          {
            addr = "[${s.tunnelCfg.sub_ipv6}]";
            port = 80;
          }
        ];
        root = "${s.clientUi}";
        locations."/" = {
          tryFiles = "$uri $uri/ /index.html";
        };
        locations."/api/" = {
          proxyPass = "http://127.0.0.1:3001";
          extraConfig = ''
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_buffering off;
            proxy_cache off;
          '';
        };
        locations."/api/node-agent/" = {
          proxyPass = "http://127.0.0.1:3002";
          extraConfig = ''
            rewrite ^/api/node-agent/(.*) /$1 break;
            proxy_http_version 1.1;
            proxy_set_header Connection "";
            proxy_buffering off;
            proxy_cache off;
          '';
        };
      };
    };

    systemd.services.yolab-local-api = {
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];
      path = [
        pkgs.git
        pkgs.nix
      ];
      environment = {
        YOLAB_REPO_PATH = config.yolab.repoPath;
        YOLAB_PLATFORM = config.yolab.platform;
        YOLAB_FLAKE_TARGET = config.yolab.flakeTarget;
      };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "5s";
        ExecStart = "${s.localApiEnv}/bin/local-api";
      };
    };

    systemd.services.yolab-node-agent = {
      after = [
        "network.target"
      ]
      ++ lib.optional s.tunnelEnabled "wireguard-wg0.service"
      ++ lib.optional s.swarmEnabled "k3s.service";
      wantedBy = [ "multi-user.target" ];
      path = with pkgs; [
        util-linux
        e2fsprogs
        nfs-utils
        mergerfs
        kubectl
        rsync
      ];
      environment = {
        NODE_ID = s.nodeCfg.node_id or "";
        WG_IPV6 = if s.tunnelEnabled then s.tunnelCfg.sub_ipv6 else "";
        WG_INTERFACE = "wg0";
        K3S_ROLE = k3sCfg.role or "server";
        YOLAB_PLATFORM = config.yolab.platform;
      };
      serviceConfig = {
        Type = "simple";
        User = "root";
        Restart = "always";
        RestartSec = "10s";
        ExecStart = "${s.nodeAgentEnv}/bin/node-agent";
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
    services.logind.lidSwitchExternalPower = "ignore";

    environment.systemPackages =
      with pkgs;
      map lib.lowPrio [
        curl
        gitMinimal
        just
        wireguard-tools
        kubectl
        mergerfs
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
