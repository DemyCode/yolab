{
  pkgs,
  lib,
  config,
  yolabConfigPath,
  rust,
  localApiEnv,
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

  tunnelDomain = lib.removePrefix "https://" (lib.removePrefix "http://" s.tunnelCfg.dns_url);
  userDomain = lib.concatStringsSep "." (lib.drop 1 (lib.splitString "." tunnelDomain));
  platformApiUrl = lib.removeSuffix "/" (s.tunnelCfg.platform_api_url or "https://api.yolab.io");

  localApi = "[::1]:${toString s.localApiPort}";
in {
  config = {
    services.caddy = {
      enable = true;
      package = pkgs.caddy.withPlugins {
        plugins = ["github.com/caddy-dns/acmedns@v0.7.0"];
        hash = "sha256-ZI3OawB18O3q9P8Ub65AUjTunLdqn316LuXIYk5Qssk=";
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
          header {
            X-Frame-Options DENY
            Content-Security-Policy "frame-ancestors 'none'; object-src 'none'; base-uri 'self'"
            X-Content-Type-Options nosniff
            Referrer-Policy no-referrer
            Strict-Transport-Security "max-age=31536000"
            -Server
          }
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
  };
}
