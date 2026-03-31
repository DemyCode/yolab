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
  isFirstNode = (k3sCfg.server_addr or "") == "";
  tunnelDomain =
    if s.tunnelEnabled then
      lib.removePrefix "https://" (lib.removePrefix "http://" (s.tunnelCfg.dns_url or ""))
    else
      "";
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
          # Accept inbound packets from any source (same as AllowedIPs = ::/0 on the installer).
          # postSetup removes the default route NixOS injects for ::/0 and replaces it with
          # source-based policy routing so only return traffic (packets *from* sub_ipv6) is
          # sent back through the tunnel — outbound internet access is unaffected.
          postSetup = ''
            ip -6 route del ::/0 dev wg0 2>/dev/null || true
            ip -6 rule add from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
            ip -6 route add ::/0 dev wg0 table 51820
          '';
          preShutdown = ''
            ip -6 rule del from ${s.tunnelCfg.sub_ipv6} lookup 51820 priority 100 2>/dev/null || true
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
    };

    services.openssh = {
      enable = true;
      ports = [ s.sshPort ];
      settings = {
        PermitRootLogin = lib.mkIf (s.rootSshKey != "") "prohibit-password";
        PasswordAuthentication = false;
      };
    };

    services.k3s = {
      enable = true;
      role = "server";
      token = k3sCfg.token;
      clusterInit = isFirstNode;
      serverAddr = lib.optionalString (!isFirstNode) (k3sCfg.server_addr or "");
      extraFlags = toString [
        "--flannel-backend=wireguard-native"
        "--advertise-address=${s.tunnelCfg.sub_ipv6}"
        "--node-ip=${s.tunnelCfg.sub_ipv6}"
      ];
    };

    services.caddy = {
      enable = true;
      configFile = pkgs.writeText "Caddyfile" (
        lib.optionalString s.tunnelEnabled ''
          ${tunnelDomain} {
            handle /api/* {
              reverse_proxy 127.0.0.1:3001
            }
            handle {
              root * ${s.clientUi}
              try_files {path} /index.html
              file_server
            }
          }
        ''
      );
    };

    systemd.services.caddy = {
      after = [ "wireguard-wg0.service" ];
      wants = [ "wireguard-wg0.service" ];
    };

    systemd.services.yolab-local-api = {
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];
      # Use the full system PATH so nixos-rebuild, git, nix and friends are all reachable.
      # The service runs as root so no sudo is needed for nixos-rebuild.
      environment = {
        PATH = lib.mkForce "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin:/run/wrappers/bin";
        YOLAB_REPO_PATH = config.yolab.repoPath;
        YOLAB_CONFIG = "${config.yolab.repoPath}/homelab/ignored/config.toml";
        YOLAB_PLATFORM = config.yolab.platform;
        YOLAB_FLAKE_TARGET = config.yolab.flakeTarget;
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
