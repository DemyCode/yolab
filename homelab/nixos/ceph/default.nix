{
  config,
  lib,
  pkgs,
  localApiEnv,
  ...
}:
with lib; let
  cfg = config.yolab.ceph;
  host = config.networking.hostName;

  isBootstrap = cfg.joinSeedAddr == "";

  addrvec = a: "[v2:[${a}]:3300,v1:[${a}]:6789]";

  monSeeds = [cfg.monAddr] ++ optional (!isBootstrap) cfg.joinSeedAddr;
in {
  options.yolab.ceph = {
    enable = mkEnableOption "host-level Ceph (outside Kubernetes)";

    fsid = mkOption {
      type = types.str;
      description = "Cluster fsid. Generated once at install time, stored in config.toml.";
    };

    monAddr = mkOption {
      type = types.str;
      description = ''
        Address this node's mon binds to and advertises. Must be the WireGuard
        cluster address — peers reach it over the tunnel, not over any LAN.
      '';
    };

    clusterSubnet = mkOption {
      type = types.str;
      default = "fd00:cafe::/112";
      description = ''
        The WireGuard mesh that every node's cluster address lives in, used as
        Ceph's public_network.

        It must be the SUBNET, never this node's own /128. public_network is how
        a daemon picks which local address to bind, and a /128 describes a
        network containing exactly one machine — which is precisely why the
        original config could not grow: every node's ceph.conf described a
        different, one-member cluster.
      '';
    };

    monInitialMembers = mkOption {
      type = types.listOf types.str;
      default = [host];
      description = ''
        Only consulted while a mon forms quorum from an *empty* monmap. Every mon
        here is created with `--mkfs --monmap`, so it always has a real one and
        this is inert; it is set to the local host to silence the upstream
        module's warning about leaving it null.
      '';
    };

    joinSeedAddr = mkOption {
      type = types.str;
      default = "";
      description = ''
        Cluster address of a machine that is already in the cluster, or "" on the
        machine that creates it.

        This is the only asymmetry in the design, and it is a one-time event
        rather than a role: it says "fetch this cluster's identity from over
        there" the first time this node boots. Once the mon store exists every
        node is an equal peer — its own mon, mgr, MDS and OSDs — and nothing
        reads this again.
      '';
    };
  };

  config = mkIf cfg.enable {
    services.ceph = {
      enable = true;
      global = {
        inherit (cfg) fsid;
        clusterName = "ceph";
        monInitialMembers = concatStringsSep "," cfg.monInitialMembers;
        monHost = concatStringsSep "," (map addrvec monSeeds);
        publicNetwork = cfg.clusterSubnet;
        authClusterRequired = "cephx";
        authServiceRequired = "cephx";
        authClientRequired = "cephx";
      };

      extraConfig = {
        ms_bind_ipv6 = "true";
        ms_bind_ipv4 = "false";

        osd_pool_default_size = "1";
        osd_pool_default_min_size = "1";
        mon_allow_pool_size_one = "true";

        osd_crush_initial_weight = "0";

        mon_osd_down_out_interval = "600";

        client_mount_timeout = "30";

        bluestore_cache_size_ssd = "1073741824";
        osd_max_backfills = "4";
        osd_recovery_max_active = "4";
      };

      mon = {
        enable = true;
        daemons = [host];
        extraConfig = {
          auth_allow_insecure_global_id_reclaim = "false";
        };
      };
      mgr = {
        enable = true;
        daemons = [host];
      };
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/ceph 0750 ceph ceph -"
      "d /var/lib/ceph/mon 0750 ceph ceph -"
      "d /var/lib/ceph/mgr 0750 ceph ceph -"
      "d /var/lib/ceph/osd 0750 ceph ceph -"
      "d /var/lib/ceph/bootstrap-osd 0750 ceph ceph -"
    ];

    systemd.services.yolab-ceph-bootstrap = {
      description =
        if isBootstrap
        then "Create the Ceph cluster (keyrings, monmap, mon mkfs)"
        else "Join the Ceph cluster (fetch keyrings, monmap, mon mkfs)";
      wantedBy = ["multi-user.target"];
      before = ["ceph-mon-${host}.service"];
      requiredBy = ["ceph-mon-${host}.service"];
      restartIfChanged = false;
      after = ["network-online.target" "wireguard-wg1.service"];
      wants = ["network-online.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "300s";
        ExecStart = "${localApiEnv}/bin/local-api storage bootstrap";
      };
      path = with pkgs; [ceph ceph-client coreutils];
      environment = {
        YOLAB_CEPH_FSID = cfg.fsid;
        YOLAB_CEPH_MON_ADDR = cfg.monAddr;
        YOLAB_CEPH_JOIN_SEED_ADDR = cfg.joinSeedAddr;
        YOLAB_CONFIG = "${config.yolab.machineDir}/config.toml";
      };
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mon-${host}.service || true
      '';
    };

    systemd.services.yolab-ceph-mon-member = mkIf (!isBootstrap) {
      description = "Ensure this node's mon is in the monmap";
      after = ["ceph-mon-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "300s";
        ExecStart = "${localApiEnv}/bin/local-api storage mon-member";
      };
      path = with pkgs; [ceph ceph-client coreutils systemd];
      environment.YOLAB_CEPH_MON_ADDR = cfg.monAddr;
    };

    systemd.services.yolab-ceph-mgr-key = {
      description = "Create the mgr auth key";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service"];
      before = ["ceph-mgr-${host}.service"];
      requiredBy = ["ceph-mgr-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "180s";
        ExecStart = "${localApiEnv}/bin/local-api storage mgr-key";
      };
      path = with pkgs; [ceph ceph-client coreutils systemd];
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mgr-${host}.service || true
      '';
    };

    systemd.services."yolab-ceph-osd@" = {
      description = "Ceph OSD %i";
      after = ["network-online.target" "ceph-mon-${host}.service"];
      wants = ["network-online.target" "ceph-mon-${host}.service"];
      restartIfChanged = false;
      path = with pkgs; [ceph ceph-client lvm2 util-linux coreutils];
      startLimitIntervalSec = 0;
      serviceConfig = {
        Type = "simple";
        Restart = "on-failure";
        RestartSec = "30s";
        TimeoutStartSec = "180s";
        ExecStartPre =
          pkgs.writeShellScript "yolab-ceph-osd-activate" ''
            set -euo pipefail
            export PATH=${lib.makeBinPath (with pkgs; [ceph ceph-client lvm2 util-linux coreutils jq])}:$PATH
            OSD_ID="$1"
            # `timeout`, because ceph-volume shells out to `lvs` and lvs reads
            # every block device it is allowed to see. If any of them does not
            # answer, lvs blocks in uninterruptible sleep, ExecStartPre never
            # returns, and SIGKILL cannot clear it.
            #
            # Seen on node1: osd.0 restarted 21 times over 35 minutes, each attempt
            # leaving another unkillable `lvs` in the unit's cgroup, and the OSD
            # never came up. It was a closed loop — osd.0 was down, so Ceph could
            # not serve the RBD, so lvs stalled scanning it, so osd.0 could not
            # start. The RBD is now excluded from LVM's scan entirely (see
            # images-store.nix), which is the actual fix; this bounds the damage if
            # some other device ever stalls the same way.
            FSID=$(timeout 60 ceph-volume lvm list "$OSD_ID" --format json 2>/dev/null \
              | jq -r --arg id "$OSD_ID" '.[$id][0].tags["ceph.osd_fsid"] // empty')
            if [ -z "$FSID" ]; then
              echo "osd.$OSD_ID: no ceph-volume metadata found for it on this host" >&2
              exit 1
            fi
            # --no-systemd stops ceph-volume generating its own competing units;
            # this template is the single supervisor for every OSD on the host.
            exec timeout 120 ceph-volume lvm activate --no-systemd "$OSD_ID" "$FSID"
          ''
          + " %i";
        ExecStart = "${pkgs.ceph}/bin/ceph-osd -f -i %i --setuser ceph --setgroup ceph";
      };
    };

    systemd.services.yolab-ceph-system-osd = {
      description = "Make this machine's system LV an OSD of the cluster";
      wantedBy = ["multi-user.target"];
      after = ["yolab-ceph-bootstrap.service" "ceph-mon-${host}.service"];
      wants = ["ceph-mon-${host}.service"];
      restartIfChanged = false;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # NOT infinity, unlike this looked before: `local-api storage
        # system-osd` retries forever with no give-up (wait::until_ready has
        # no bound) when /dev/mapper/pool-ceph does not exist — a real,
        # tolerated state for "this machine was not installed with the YoLab
        # disk layout" (see the error text in disks_reconciler.rs, and
        # nix/tests/disk-loss.nix, whose node never has a system LV at all).
        # yolab-images-rbd is explicitly After=/Wants= this unit (see
        # containerd-store-after-order in nix/checks.nix), so a start job
        # that never reaches a terminal state here holds up the ENTIRE k3s
        # boot line behind it forever, not just this one OSD.
        #
        # A bounded timeout still fails loudly — systemd marks the unit
        # failed, which `systemctl --failed` shows, matching the existing
        # a_machine_without_the_system_lv_is_an_error_not_a_skip Rust test —
        # it just also lets ordering resolve so images-rbd, containerd-store
        # and k3s can come up regardless, exactly like
        # yolab-ceph-osd-activate's own bounded 600s just below.
        TimeoutStartSec = "600s";
        ExecStart = "${localApiEnv}/bin/local-api storage system-osd";
      };
      path = with pkgs; [ceph ceph-client lvm2 util-linux coreutils systemd];
    };

    systemd.services.yolab-ceph-osd-activate = {
      description = "Start a yolab-ceph-osd@ instance for every OSD prepared on this host";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "600s";
        ExecStart = "${localApiEnv}/bin/local-api storage osd-activate";
      };
      path = with pkgs; [ceph ceph-client lvm2 util-linux coreutils systemd];
    };

    environment.systemPackages = with pkgs; [
      ceph
      ceph-client
      xfsprogs
      lvm2
      util-linux
    ];

    boot.kernelModules = ["rbd" "libceph"];
  };
}
