# CephFS — the filesystem behind every app's PVC.
#
# Rook used to declare this as a CephFilesystem CR and its operator created the
# pools, ran `ceph fs new`, and scheduled the MDS. With Ceph on the host all
# three become ours.
#
# The filesystem name, pool names and layout deliberately match what the Rook
# CephFilesystem produced (`yolab-fs`, `yolab-fs-metadata`, `yolab-fs-data0`),
# because the `yolab-cephfs` StorageClass and every existing PV reference those
# names. Changing them would orphan real volumes.
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
      # Like mon and mgr, the MDS id is just the hostname, so it can be static.
      daemons = [host];
    };

    # The MDS refuses to start without its own cephx key, and only the mon can
    # mint one — same ordering problem as the mgr.
    systemd.services.yolab-ceph-mds-key = {
      description = "Create the MDS auth key";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service"];
      before = ["ceph-mds-${host}.service"];
      requiredBy = ["ceph-mds-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        # NOT RemainAfterExit — see the same note on yolab-ceph-mgr-key: it lets
        # each ceph-mds start re-run this, and `requiredBy` on ceph-mds holds
        # fine without it.
        #
        # `Type=oneshot` disables the start timeout by default; see the note on
        # yolab-ceph-bootstrap in default.nix for what that cost.
        TimeoutStartSec = "180s";
        ExecStart = "${localApiEnv}/bin/local-api storage mds-key";
      };
      path = with pkgs; [ceph ceph-client coreutils systemd];
      # At boot systemd already orders ceph-mds after this; this matters on a
      # retry, where the failed first attempt took the MDS's start job with it.
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mds-${host}.service || true
      '';
    };

    # Re-run by the `ceph-keys` controller (controllers.rs), which mints the MDS
    # key and starts the daemon.

    systemd.tmpfiles.rules = [
      "d /var/lib/ceph/mds 0750 ceph ceph -"
    ];
  };
}
