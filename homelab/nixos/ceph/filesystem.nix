{
  config,
  lib,
  pkgs,
  localApiEnv,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.filesystem;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;
in {
  options.yolab.ceph.filesystem = {
    enable = mkEnableOption "CephFS for PVC storage";
  };

  config = mkIf (cephCfg.enable && cfg.enable) {
    services.ceph.mds = {
      enable = true;
      daemons = [host];
    };

    systemd.services.yolab-ceph-mds-key = {
      description = "Create the MDS auth key";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service"];
      before = ["ceph-mds-${host}.service"];
      requiredBy = ["ceph-mds-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "180s";
        ExecStart = "${localApiEnv}/bin/local-api storage mds-key";
      };
      path = with pkgs; [ceph ceph-client coreutils systemd];
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mds-${host}.service || true
      '';
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/ceph/mds 0750 ceph ceph -"
    ];
  };
}
