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
      };
      path = with pkgs; [ceph ceph-client coreutils];
      script = ''
        set -euo pipefail
        MDS_DIR=/var/lib/ceph/mds/ceph-${host}
        [ -f "$MDS_DIR/keyring" ] && exit 0
        mkdir -p "$MDS_DIR"
        for _ in $(seq 1 60); do
          ceph -s >/dev/null 2>&1 && break
          sleep 1
        done
        ceph auth get-or-create mds.${host} \
          mon 'profile mds' mgr 'profile mds' osd 'allow rwx' mds 'allow *' \
          -o "$MDS_DIR/keyring"
        chown -R ceph:ceph "$MDS_DIR"
      '';
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/ceph/mds 0750 ceph ceph -"
    ];

    # ── Create the filesystem ────────────────────────────────────────────────
    # Like the images pool, this cannot run until an OSD exists, and on a fresh
    # cluster none do until the user switches a disk on. So it exits cleanly
    # when it cannot proceed and a timer retries, rather than failing the boot.
    systemd.services.yolab-cephfs-init = {
      description = "Create the ${cfg.name} CephFS and its pools";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service" "ceph-mgr-${host}.service"];
      serviceConfig.Type = "oneshot";
      path = with pkgs; [ceph ceph-client coreutils gnugrep];
      script = ''
        set -uo pipefail
        for _ in $(seq 1 90); do ceph -s >/dev/null 2>&1 && break; sleep 1; done
        if ! ceph -s >/dev/null 2>&1; then
          echo "ceph not reachable — nothing to create yet"
          exit 0
        fi
        if ! ceph osd stat 2>/dev/null | grep -qE '[1-9][0-9]* up'; then
          echo "no OSD is up yet — ${cfg.name} will be created once a disk is switched on"
          exit 0
        fi

        if ceph fs ls --format json 2>/dev/null | grep -q '"name":"${cfg.name}"'; then
          echo "${cfg.name} already exists"
          exit 0
        fi

        set -e
        # Pool names are load-bearing: the yolab-cephfs StorageClass names
        # ${cfg.name}-data0 explicitly, and existing PVs reference it.
        ceph osd pool ls | grep -qx ${cfg.name}-metadata || ceph osd pool create ${cfg.name}-metadata 16 16
        ceph osd pool ls | grep -qx ${cfg.name}-data0    || ceph osd pool create ${cfg.name}-data0 32 32

        # size=1 is the single-node floor. topology.rs raises both pools as
        # nodes join; it is not the intended steady state for a real cluster.
        ceph osd pool set ${cfg.name}-metadata size 1 --yes-i-really-mean-it
        ceph osd pool set ${cfg.name}-data0 size 1 --yes-i-really-mean-it

        ceph fs new ${cfg.name} ${cfg.name}-metadata ${cfg.name}-data0 --force
        echo "created CephFS ${cfg.name}"
      '';
    };

    systemd.timers.yolab-cephfs-init = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "3min";
        OnUnitActiveSec = "5min";
      };
    };
  };
}
