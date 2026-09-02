# The Ceph dashboard, served from the host mgr instead of Rook.
#
# Not a reverse proxy to [::1]:7000: the dashboard runs on the ACTIVE mgr only,
# and a standby redirects to the active one's WireGuard address, which is
# unreachable from a browser. Whether proxying locally worked would depend on
# which machine happened to hold the active mgr.
#
# local-api asks Ceph which mgr is active (`ceph mgr services`) and forwards
# there over the mesh, so failover changes the answer and nothing else notices.
{
  config,
  lib,
  pkgs,
  localApiEnv,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.dashboard;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;
  cephPath = with pkgs; [ceph ceph-client];
in {
  options.yolab.ceph.dashboard = {
    enable = mkEnableOption "the Ceph dashboard on this node's mgr" // {default = true;};

    port = mkOption {
      type = types.port;
      default = 7000;
      description = "Port the mgr dashboard listens on, on the cluster address.";
    };

    urlPrefix = mkOption {
      type = types.str;
      default = "/ceph-dashboard";
      description = ''
        Sub-path the dashboard is served under. It has to be told this: the
        dashboard is a single-page app that builds its own asset and API URLs,
        and without a prefix it generates links rooted at "/" that miss the
        proxy entirely and land on the YoLab UI instead.
      '';
    };

    passwordFile = mkOption {
      type = types.path;
      default = "/var/lib/ceph/dashboard-password";
      description = ''
        Where the generated admin password is kept. local-api reads this to show
        the credentials on the Storage page, so the two can never disagree —
        which is what happened when the password lived in a Kubernetes Secret
        and the dashboard did not.
      '';
    };
  };

  config = mkIf (cephCfg.enable && cfg.enable) {
    systemd.services.yolab-ceph-dashboard = {
      description = "Enable and configure the Ceph dashboard";
      after = ["ceph-mgr-${host}.service"];
      wantedBy = ["multi-user.target"];
      serviceConfig = {
        Type = "oneshot";
        # `Type=oneshot` disables the start timeout by default; see the note on
        # yolab-ceph-bootstrap in default.nix.
        TimeoutStartSec = "300s";
        ExecStart = "${localApiEnv}/bin/local-api storage dashboard";
      };
      # The restart-needed comparison (compared against what the mgr REPORTS
      # it serves, never `ceph config get` — see homelab/local-api/src/storage
      # /dashboard.rs's header for why), the cluster-wide password
      # generation/adoption/race-resolution, and the login-verify-and-reapply
      # loop all live in that module now, with unit tests for each decision.
      path = cephPath;
      environment = {
        YOLAB_CEPH_DASHBOARD_PORT = toString cfg.port;
        YOLAB_CEPH_DASHBOARD_PREFIX = cfg.urlPrefix;
        YOLAB_CEPH_DASHBOARD_PASSWORD_FILE = cfg.passwordFile;
        YOLAB_CEPH_MON_ADDR = cephCfg.monAddr;
      };
    };

    # Re-assert periodically. A mgr failover moves the dashboard to another
    # node, and a node that has never been active has still had its config set
    # here, so the move needs nothing to happen. This is for the case where the
    # module got disabled or the config was cleared by hand.
    systemd.timers.yolab-ceph-dashboard = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "4min";
        OnUnitActiveSec = "30min";
        # A failed attempt never reaches the active state OnUnitActiveSec
        # measures from — see the note on yolab-ceph-mgr-key's timer.
        OnUnitInactiveSec = "2min";
      };
    };
  };
}
