{ pkgs, lib, inputs, ... }:
let
  s = import ../shared.nix { inherit pkgs lib inputs; };
  k3sCfg = s.nodeCfg.k3s or { };

  wg0Conf = pkgs.writeText "wg0.conf" (lib.optionalString s.tunnelEnabled ''
    [Interface]
    PrivateKey = ${s.tunnelCfg.wg_private_key}
    Address = ${s.tunnelCfg.sub_ipv6}/128

    [Peer]
    PublicKey = ${s.tunnelCfg.wg_server_public_key}
    Endpoint = ${s.tunnelCfg.wg_server_endpoint}
    AllowedIPs = ${s.wgSubnet}
    PersistentKeepalive = 25
  '');

  limaK3sTemplate = pkgs.writeText "lima-k3s.yaml" ''
    vmType: vz
    memory: "2GiB"
    disk: "20GiB"
    images:
      - location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-amd64.img"
        arch: "x86_64"
      - location: "https://cloud-images.ubuntu.com/releases/24.04/release/ubuntu-24.04-server-cloudimg-arm64.img"
        arch: "aarch64"
    networks:
      - vzNAT: true
    provision:
      - mode: system
        script: |
          #!/bin/bash
          set -e
          curl -sfL https://get.k3s.io | \
            K3S_URL="${k3sCfg.server_addr or ""}" \
            K3S_TOKEN="${k3sCfg.token or ""}" \
            sh -s - agent \
            --node-ip=$(ip -4 addr show lima0 | grep -oP '(?<=inet\s)\d+(\.\d+){3}')
  '';
in
{
  networking.hostName = s.hostname;
  time.timeZone = s.timezone;

  nix.settings.experimental-features = [ "nix-command" "flakes" ];
  nix.gc.automatic = true;
  services.nix-daemon.enable = true;

  environment.systemPackages = with pkgs; [
    curl
    git
    vim
    wget
    htop
    lima
    wireguard-go
    wireguard-tools
  ];

  environment.etc."yolab/nginx.conf".text = ''
    events {}
    http {
      include ${pkgs.nginx}/conf/mime.types;
      server {
        listen 0.0.0.0:80;
        ${lib.optionalString s.tunnelEnabled "listen [${s.tunnelCfg.sub_ipv6}]:80;"}
        root ${s.clientUi};
        location / { try_files $uri $uri/ /index.html; }
        location /api/ {
          proxy_pass http://127.0.0.1:3001;
          proxy_http_version 1.1;
          proxy_set_header Connection "";
          proxy_buffering off;
          proxy_cache off;
        }
        location /api/node-agent/ {
          rewrite ^/api/node-agent/(.*) /$1 break;
          proxy_pass http://127.0.0.1:3002;
          proxy_http_version 1.1;
          proxy_set_header Connection "";
          proxy_buffering off;
          proxy_cache off;
        }
      }
    }
  '';

  launchd.daemons.yolab-nginx = {
    serviceConfig = {
      ProgramArguments = [
        "${pkgs.nginx}/bin/nginx"
        "-c" "/etc/yolab/nginx.conf"
        "-g" "daemon off;"
      ];
      RunAtLoad = true;
      KeepAlive = true;
      StandardOutPath = "/var/log/yolab-nginx.log";
      StandardErrorPath = "/var/log/yolab-nginx-error.log";
    };
  };

  launchd.daemons.yolab-local-api = {
    serviceConfig = {
      ProgramArguments = [ "${s.localApiEnv}/bin/local-api" ];
      RunAtLoad = true;
      KeepAlive = true;
      StandardOutPath = "/var/log/yolab-local-api.log";
      StandardErrorPath = "/var/log/yolab-local-api-error.log";
      EnvironmentVariables = {
        YOLAB_REPO_PATH    = s.repoPath;
        YOLAB_PLATFORM     = "darwin";
        YOLAB_FLAKE_TARGET = s.flakeTarget;
        PATH               = "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/bin:/bin";
      };
    };
  };

  launchd.daemons.yolab-node-agent = {
    serviceConfig = {
      ProgramArguments = [ "${s.nodeAgentEnv}/bin/node-agent" ];
      RunAtLoad = true;
      KeepAlive = true;
      StandardOutPath = "/var/log/yolab-node-agent.log";
      StandardErrorPath = "/var/log/yolab-node-agent-error.log";
      EnvironmentVariables = {
        NODE_ID        = s.nodeCfg.node_id or "";
        WG_IPV6        = lib.optionalString s.tunnelEnabled s.tunnelCfg.sub_ipv6;
        WG_INTERFACE   = "wg0";
        K3S_ROLE       = k3sCfg.role or "agent";
        YOLAB_PLATFORM = "darwin";
        PATH           = "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/usr/bin:/bin";
      };
    };
  };

  launchd.daemons.yolab-lima-k3s = lib.mkIf s.swarmEnabled {
    serviceConfig = {
      ProgramArguments = [
        "/bin/sh" "-c"
        ''
          ${pkgs.lima}/bin/limactl start --name=yolab-k3s ${limaK3sTemplate} 2>/dev/null || \
          ${pkgs.lima}/bin/limactl start yolab-k3s
        ''
      ];
      RunAtLoad = true;
      KeepAlive = false;
      StandardOutPath = "/var/log/yolab-lima-k3s.log";
      StandardErrorPath = "/var/log/yolab-lima-k3s-error.log";
    };
  };

  launchd.daemons.yolab-wireguard = lib.mkIf s.tunnelEnabled {
    serviceConfig = {
      ProgramArguments = [
        "/bin/sh" "-c"
        ''
          export WG_QUICK_USERSPACE_IMPLEMENTATION=${pkgs.wireguard-go}/bin/wireguard-go
          ${pkgs.wireguard-tools}/bin/wg-quick down ${wg0Conf} 2>/dev/null || true
          ${pkgs.wireguard-tools}/bin/wg-quick up ${wg0Conf}
        ''
      ];
      RunAtLoad = true;
      KeepAlive = false;
      StandardOutPath = "/var/log/yolab-wireguard.log";
      StandardErrorPath = "/var/log/yolab-wireguard-error.log";
    };
  };

  system.stateVersion = 5;
}
