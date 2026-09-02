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
  ...
}:
with lib; let
  cfg = config.yolab.ceph.filesystem;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;
in {
  options.yolab.ceph.filesystem = {
    enable = mkEnableOption "CephFS for PVC storage";

    name = mkOption {
      type = types.str;
      default = "yolab-fs";
      description = "Must match the fsName in the yolab-cephfs StorageClass.";
    };
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
        RemainAfterExit = true;
        # `Type=oneshot` disables the start timeout by default; see the note on
        # yolab-ceph-bootstrap in default.nix for what that cost.
        TimeoutStartSec = "180s";
      };
      path = with pkgs; [ceph ceph-client coreutils systemd];
      # At boot systemd already orders ceph-mds after this; this matters on a
      # retry, where the failed first attempt took the MDS's start job with it.
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mds-${host}.service || true
      '';
      script = ''
        set -euo pipefail
        MDS_DIR=/var/lib/ceph/mds/ceph-${host}
        [ -f "$MDS_DIR/keyring" ] && exit 0
        mkdir -p "$MDS_DIR"
        for _ in $(seq 1 60); do
          ceph -s >/dev/null 2>&1 && break
          sleep 1
        done
        # Only the mon can mint this, so an unreachable cluster means "not yet".
        # On a joining node that is the ordinary state until the cluster hands
        # over its credentials — hence the retry timer below.
        if ! timeout 20 ceph -s >/dev/null 2>&1; then
          echo "cluster not reachable — cannot mint the MDS key yet" >&2
          exit 1
        fi
        ceph auth get-or-create mds.${host} \
          mon 'profile mds' mgr 'profile mds' osd 'allow rwx' mds 'allow *' \
          -o "$MDS_DIR/keyring"
        chown -R ceph:ceph "$MDS_DIR"
      '';
    };

    # Without this a joining node's MDS stays down until the next reboot: a
    # failed oneshot is never retried on its own, and on a joining node the
    # first attempt necessarily runs before the cluster credentials arrive.
    systemd.timers.yolab-ceph-mds-key = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "2min";
        OnUnitActiveSec = "5min";
        OnUnitInactiveSec = "2min";
      };
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/ceph/mds 0750 ceph ceph -"
    ];

  };
}
