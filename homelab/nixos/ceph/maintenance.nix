{
  config,
  lib,
  pkgs,
  localApiEnv,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.maintenance;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;
in {
  options.yolab.ceph.maintenance.enable =
    mkEnableOption "noout on reboot and health-gated Ceph restarts";

  config = mkIf (cephCfg.enable && cfg.enable) {
    systemd.services.yolab-ceph-noout = {
      description = "Hold Ceph's noout flag across a reboot";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "120s";
        TimeoutStopSec = "60s";
        ExecStart = "${localApiEnv}/bin/local-api storage noout-clear";
        ExecStop = "${localApiEnv}/bin/local-api storage noout-set";
      };
      path = with pkgs; [ceph ceph-client coreutils];
    };
  };
}
