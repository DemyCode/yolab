# Guards that stop routine maintenance from becoming a storage incident.
#
# Under Rook these were the operator's job. With Ceph on the host — and with
# this product auto-updating itself via nixos-rebuild — they are ours, and they
# are not optional: a rebuild that bumps the Ceph package restarts every daemon
# on the machine as a side effect.
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
    # ── noout across reboots ─────────────────────────────────────────────────
    # An OSD that stays down for mon_osd_down_out_interval (600s) is marked
    # `out`, and Ceph starts copying all of its data onto the remaining disks to
    # restore replica count. That is right for a dead disk and wrong for a
    # reboot, where it comes back in two minutes. With osd_max_backfills=4 the
    # pointless rebalance is aggressive, and on a multi-node cluster it saturates
    # the WireGuard links copying data that was never lost.
    #
    # Note this risk predates host-level Ceph — a node reboot took Rook's OSD
    # pods down exactly the same way, and nothing set noout then either. It
    # matters more now only because reboots are routine.
    systemd.services.yolab-ceph-noout = {
      description = "Hold Ceph's noout flag across a reboot";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # `Type=oneshot` disables the start timeout by default. TimeoutStopSec
        # matters even more here: ExecStop runs at shutdown and talks to the
        # cluster, so an unbounded one hangs the reboot — the reboot being the
        # thing that recovers a machine whose storage is stuck.
        TimeoutStartSec = "120s";
        TimeoutStopSec = "60s";
        # Nothing to do on start beyond clearing the flag the shutdown set.
        # The reachability wait, the "only if we set it" check and the
        # "already set by someone else" guard all live in
        # homelab/local-api/src/storage/noout.rs now, with unit tests for
        # both directions.
        ExecStart = "${localApiEnv}/bin/local-api storage noout-clear";
        ExecStop = "${localApiEnv}/bin/local-api storage noout-set";
      };
      path = with pkgs; [ceph ceph-client coreutils];
    };
  };
}
