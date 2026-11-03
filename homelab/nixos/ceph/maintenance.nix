{
  config,
  pkgs,
  localApiEnv,
  ...
}:
let
  host = config.networking.hostName;
in {
  config = {
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
