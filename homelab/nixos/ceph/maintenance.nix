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
        ExecStart = pkgs.writeShellScript "ceph-noout-clear" ''
          set -uo pipefail
          export PATH=${makeBinPath (with pkgs; [ceph ceph-client coreutils])}:$PATH
          # Wait for the mon, but never block boot on it.
          for _ in $(seq 1 60); do timeout 15 ceph -s >/dev/null 2>&1 && break; sleep 1; done
          if ! timeout 15 ceph -s >/dev/null 2>&1; then
            echo "ceph unreachable — leaving flags alone"
            exit 0
          fi
          # Only clear noout if *we* set it. An operator may have set it by hand
          # for a maintenance window that outlives this reboot, and stomping that
          # would resume rebalancing in the middle of their work.
          if [ -f /var/lib/ceph/.yolab-set-noout ]; then
            ceph osd unset noout || true
            rm -f /var/lib/ceph/.yolab-set-noout
            echo "cleared noout"
          fi
        '';
        ExecStop = pkgs.writeShellScript "ceph-noout-set" ''
          set -uo pipefail
          export PATH=${makeBinPath (with pkgs; [ceph ceph-client coreutils gnugrep])}:$PATH
          timeout 15 ceph -s >/dev/null 2>&1 || exit 0
          # Don't touch it if it is already set — see above.
          if ceph osd dump 2>/dev/null | grep -q '^flags.*noout'; then
            echo "noout already set by someone else — leaving it"
            exit 0
          fi
          ceph osd set noout || true
          touch /var/lib/ceph/.yolab-set-noout
          echo "set noout for shutdown"
        '';
      };
    };

    # ── Health-gated Ceph restarts ───────────────────────────────────────────
    # rook/cluster.yaml carried an explicit warning that a Ceph version bump must
    # gate on health "so a bad upgrade can't proceed through an unhealthy cluster
    # and get stuck half-migrated". Rook enforced that. Now this does.
    #
    # The dangerous case is a rolling auto-update: node2's OSDs restart while
    # node1's are still backfilling from their own restart. At size=3/min_size=2
    # that can drop PGs below min_size, and every app's I/O blocks cluster-wide
    # until recovery catches up.
    #
    # This is exposed as a script rather than wired into a unit because the
    # update path (routers/update.rs) is what must call it, before it rebuilds.
    environment.systemPackages = [
      (pkgs.writeShellScriptBin "yolab-ceph-wait-healthy" ''
        set -uo pipefail
        export PATH=${makeBinPath (with pkgs; [ceph ceph-client coreutils jq])}:$PATH

        TIMEOUT=''${1:-900}
        DEADLINE=$(( $(date +%s) + TIMEOUT ))

        if ! timeout 15 ceph -s >/dev/null 2>&1; then
          # No cluster to protect. A single node that has not set up storage yet
          # must not be blocked from updating.
          echo "ceph unreachable — not gating"
          exit 0
        fi

        while :; do
          # Wait only for data that is actually MOVING. `undersized` and
          # `degraded` are placement conditions, not movement: a pool with
          # size=2 and one usable OSD is undersized forever, and gating on that
          # blocks every update permanently — seen live, with 81 undersized PGs
          # that could never heal and a gate that would have burned its whole
          # timeout before refusing.
          DEGRADED=$(ceph pg stat -f json 2>/dev/null \
            | jq -r '(.pg_summary.num_pg_by_state // [])
                     | map(select(.name | test("backfill|recovering|peering")))
                     | map(.num) | add // 0')
          DEGRADED=''${DEGRADED:-0}

          if [ "$DEGRADED" -eq 0 ]; then
            echo "ceph has no PGs recovering — safe to restart daemons"
            exit 0
          fi

          if [ "$(date +%s)" -ge "$DEADLINE" ]; then
            echo "still $DEGRADED PG(s) recovering after ''${TIMEOUT}s — refusing to proceed" >&2
            exit 1
          fi
          echo "waiting: $DEGRADED PG(s) still recovering"
          sleep 15
        done
      '')
    ];
  };
}
