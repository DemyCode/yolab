# Ceph as host-level systemd daemons, outside Kubernetes.
#
# Forced by one constraint: adding a disk should grow the space for *container
# images*, which means containerd's data-root on a Ceph RBD. That is impossible
# with Ceph inside Kubernetes — mapping an RBD needs a mon, under Rook the mon
# is a pod, a pod needs containerd, containerd needs the RBD. On a single node
# there is nothing to break the cycle, and no ordering trick escapes it because
# containerd has exactly one image store. Host daemons need no container
# runtime, so they are up long before containerd.
#
# services.ceph is daemon *supervision* only — it generates units from static
# id lists and writes ceph.conf, with no ceph-volume, --mkfs or keyring
# generation. Hence the split:
#   - mon/mgr ids are the hostname, known at build time -> declared here.
#   - cluster bootstrap (keyrings, monmap, mon --mkfs) -> yolab-ceph-bootstrap.
#   - OSD ids are allocated by Ceph at creation, so they can never be static.
#     local-api creates them at runtime from the yolab-disk-config ConfigMap;
#     this module only re-activates prepared OSDs at boot.
#
# Every node is a peer. The first machine only *creates* the cluster, which is
# an event and not a role it keeps. The cost is stated because it cannot be
# designed away: mons agree by majority, so two machines means both must be up.
# Accepted deliberately — two copies across two hosts already leaves you at
# min_size, and three machines is where survival starts.
#
# Lost quorum shows up as every `ceph` command hanging. If a machine is merely
# off, turn it on. If it is gone for good, edit the monmap by hand:
#
#   systemctl stop ceph-mon-$(hostname)
#   ceph-mon -i $(hostname) --extract-monmap /tmp/monmap
#   monmaptool /tmp/monmap --rm <name-of-the-dead-mon>
#   ceph-mon -i $(hostname) --inject-monmap /tmp/monmap
#   systemctl start ceph-mon-$(hostname)
#
# Deliberately not automated: an offline node is indistinguishable from a
# departed one, so an automatic version would let a rebooting machine be
# evicted by its peer.
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

  # The one-time asymmetry. Creating a cluster and joining one are different
  # operations; being a member is not.
  isBootstrap = cfg.joinSeedAddr == "";

  # Ceph's address form: one bracketed group per mon, v2 and v1 inside it.
  addrvec = a: "[v2:[${a}]:3300,v1:[${a}]:6789]";

  # mon_host is a *seed* list, not the membership list — a client contacts any
  # entry and is handed the real monmap. So this only has to name one reachable
  # mon, and listing self plus the machine we joined through is enough for every
  # cluster size. Membership itself lives in the monmap, which stays correct as
  # mons come and go without any node needing a rebuild.
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
        # The whole WireGuard mesh, not this node's own /128 — see clusterSubnet.
        publicNetwork = cfg.clusterSubnet;
        authClusterRequired = "cephx";
        authServiceRequired = "cephx";
        authClientRequired = "cephx";
      };

      extraConfig = {
        # IPv6-only, matching the WireGuard cluster network. Mirrors what the
        # Rook CephCluster set; get this wrong and mons bind to an address no
        # peer can reach.
        ms_bind_ipv6 = "true";
        ms_bind_ipv4 = "false";

        # A single-node cluster cannot replicate. topology.rs raises these as
        # nodes join — they are the floor, not the target.
        osd_pool_default_size = "1";
        osd_pool_default_min_size = "1";
        mon_allow_pool_size_one = "true";

        # mon_warn_on_pool_no_redundancy is deliberately NOT disabled.
        #
        # Setting it false silences a permanent warning on a one-disk cluster,
        # but the setting is global, so it also silenced the case that matters:
        # several disks, still one copy. routers/ceph.rs translates
        # POOL_NO_REDUNDANCY into plain language, and with the check off it
        # could never fire — a three-disk cluster with one copy of everything
        # reported HEALTH_OK. Observed live.
        #
        # On a genuinely single-disk machine the warning is true, and the one
        # thing its owner most needs to know before a disk dies.

        # New OSDs attract no data until explicitly activated from the UI.
        # Carried over from the Rook config so the disk ON/OFF flow behaves the
        # same way it always has.
        osd_crush_initial_weight = "0";

        # Ten minutes down before an OSD is marked out: long enough that a
        # reboot does not trigger a rebalance, short enough that a genuinely
        # dead disk does. Reboots must still set noout — this only bounds the
        # damage when something forgets.
        mon_osd_down_out_interval = "600";

        # How long a client waits to reach a mon before giving up. The default
        # is 300s, which turns "the other machine is rebooting" into a five
        # minute hang inside every unit that shells out to `ceph` — including
        # yolab-containerd-store, which k3s is ordered after. Once there is more
        # than one mon an unreachable quorum is an ordinary transient state, so
        # it has to fail fast and be retried rather than block a boot.
        client_mount_timeout = "30";

        # Cap BlueStore's cache so an OSD is never OOM-killed mid-write; a
        # SIGKILL can leave the BlueStore label half-written and unrecoverable.
        bluestore_cache_size_ssd = "1073741824";
        osd_max_backfills = "4";
        osd_recovery_max_active = "4";
      };

      mon = {
        enable = true;
        daemons = [host];
        extraConfig = {
          # Ceph warns permanently (HEALTH_WARN) while this is on, because a
          # pre-Pacific client could reclaim a global_id insecurely. Every
          # client here is current — the daemons are one pinned build, and the
          # only kernel client is krbd on this same host — so there is nothing
          # old to break, and leaving it on means the cluster never reports
          # HEALTH_OK. A permanent warning is worse than no warning: it trains
          # you to ignore the health line, which is where a real fault appears.
          auth_allow_insecure_global_id_reclaim = "false";
        };
      };
      mgr = {
        enable = true;
        daemons = [host];
      };
      # osd.enable is deliberately unset: see the header. OSD units are created
      # at runtime by local-api because Ceph allocates their ids.
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/ceph 0750 ceph ceph -"
      "d /var/lib/ceph/mon 0750 ceph ceph -"
      "d /var/lib/ceph/mgr 0750 ceph ceph -"
      "d /var/lib/ceph/osd 0750 ceph ceph -"
      "d /var/lib/ceph/bootstrap-osd 0750 ceph ceph -"
    ];

    # ── Create or join the cluster ───────────────────────────────────────────
    #
    # Both paths end at the same place: a mon store in /var/lib/ceph/mon that
    # this node owns. From there on the two machines are indistinguishable —
    # each runs its own mon, mgr, MDS and OSDs, and none is a master.
    #
    # This unit is `before` + `requiredBy` ceph-mon-<host>, so the mon daemon
    # cannot start until it succeeds. That is also why the join path *fails*
    # rather than exiting 0 when the seed is unreachable: a failed unit keeps
    # the mon from starting against a store that was never created, and the
    # retry timer below re-runs it until the other machine answers.
    systemd.services.yolab-ceph-bootstrap = {
      description =
        if isBootstrap
        then "Create the Ceph cluster (keyrings, monmap, mon mkfs)"
        else "Join the Ceph cluster (fetch keyrings, monmap, mon mkfs)";
      wantedBy = ["multi-user.target"];
      before = ["ceph-mon-${host}.service"];
      requiredBy = ["ceph-mon-${host}.service"];
      after = ["network-online.target" "wireguard-wg1.service"];
      wants = ["network-online.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # `Type=oneshot` disables the start timeout by default — it is the one
        # service type systemd leaves unbounded. Every oneshot in this directory
        # now sets it explicitly, because one of them hanging forever is not a
        # theoretical risk: on node3 `rbd ls` blocked on an OSD read that could
        # never complete, and k3s, ordered behind it, never started.
        TimeoutStartSec = "300s";
        ExecStart = "${localApiEnv}/bin/local-api storage bootstrap";
      };
      # The script body — create-or-join, the fsid check, the monmap fetch —
      # now lives in homelab/local-api/src/storage/bootstrap.rs, with unit
      # tests covering both paths and the fsid-mismatch refusal (the one check
      # in this whole file that must never be skipped: see that module's
      # header). Only the binaries it shells out to are still named here.
      path = with pkgs; [ceph ceph-client coreutils];
      environment = {
        YOLAB_CEPH_FSID = cfg.fsid;
        YOLAB_CEPH_MON_ADDR = cfg.monAddr;
        # "" on the machine that creates the cluster.
        YOLAB_CEPH_JOIN_SEED_ADDR = cfg.joinSeedAddr;
        YOLAB_CONFIG = "${config.yolab.repoPath}/homelab/ignored/config.toml";
      };
      # At boot this is redundant — systemd already orders ceph-mon after us.
      # It matters on a *retry*: when the first attempt failed, the mon's start
      # job was dropped with it, so nothing would bring the daemon up when a
      # later attempt finally succeeds. --no-block because the mon must not be
      # waited on from inside a unit it is ordered after.
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mon-${host}.service || true
      '';
    };

    # Retry, for joining nodes only. The other machine can be rebooting, or its
    # WireGuard may not be up yet — neither is a fault, and neither should mean
    # a machine sits permanently outside the storage cluster until someone
    # notices. The bootstrap node has nothing to retry: its work is local.
    #
    # OnUnitInactiveSec as well as OnUnitActiveSec because a *failed* attempt
    # never reaches the active state, and OnUnitActiveSec alone would therefore
    # never fire again. Once the unit succeeds, RemainAfterExit leaves it active
    # and further starts are no-ops.
    systemd.timers.yolab-ceph-bootstrap = mkIf (!isBootstrap) {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "1min";
        OnUnitActiveSec = "2min";
        OnUnitInactiveSec = "2min";
      };
    };

    # ── Quorum membership ────────────────────────────────────────────────────
    # Having a mon store and a running daemon is not the same as being in the
    # monmap. A starting mon that is absent from the map asks the leader to add
    # it (MMonJoin), which is normally all that is needed; this is the explicit
    # fallback for when that has not happened, plus the place where the ordering
    # rule below is enforced.
    #
    # THE ORDERING RULE: never touch the monmap while this node's own mon is
    # down. Adding a mon raises the quorum requirement immediately — on a
    # one-mon cluster the majority goes from 1 to 2 — so the new mon must
    # already be running and able to sync within seconds. Adding it first and
    # starting it afterwards would leave the cluster with no quorum and no way
    # to run `ceph mon remove`, because that command needs the quorum it just
    # lost.
    systemd.services.yolab-ceph-mon-member = mkIf (!isBootstrap) {
      description = "Ensure this node's mon is in the monmap";
      after = ["ceph-mon-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "300s";
        ExecStart = "${localApiEnv}/bin/local-api storage mon-member";
      };
      # THE ORDERING RULE (never touch the monmap while this node's own mon is
      # down) and the retry/confirm loops now live in
      # homelab/local-api/src/storage/mon_member.rs, with unit tests covering
      # each branch — including that the monmap is never touched while the
      # local mon is inactive.
      path = with pkgs; [ceph ceph-client coreutils systemd];
      environment.YOLAB_CEPH_MON_ADDR = cfg.monAddr;
    };

    systemd.timers.yolab-ceph-mon-member = mkIf (!isBootstrap) {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "3min";
        OnUnitActiveSec = "5min";
      };
    };
    # ── mgr key ──────────────────────────────────────────────────────────────
    # The mgr refuses to start without its own cephx key, and only the mon can
    # mint one — so this has to happen between the two.
    systemd.services.yolab-ceph-mgr-key = {
      description = "Create the mgr auth key";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service"];
      before = ["ceph-mgr-${host}.service"];
      requiredBy = ["ceph-mgr-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "180s";
        ExecStart = "${localApiEnv}/bin/local-api storage mgr-key";
      };
      path = with pkgs; [ceph ceph-client coreutils systemd];
      # Same reason as yolab-ceph-bootstrap's: at boot systemd already orders
      # ceph-mgr after this, but if this attempt failed the mgr's start job died
      # with it, and nothing would bring the daemon up when the retry succeeds.
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mgr-${host}.service || true
      '';
    };

    # The mon may not exist yet — on a joining node it does not until the
    # cluster hands over its credentials. Without this the mgr would stay down
    # until the next reboot, because a failed oneshot is never retried on its
    # own. OnUnitInactiveSec as well, because a failed unit never reaches the
    # active state that OnUnitActiveSec measures from.
    systemd.timers.yolab-ceph-mgr-key = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "2min";
        OnUnitActiveSec = "5min";
        OnUnitInactiveSec = "2min";
      };
    };

    # ── OSD daemons ──────────────────────────────────────────────────────────
    # A systemd *template* unit, because Ceph assigns an OSD its id at creation
    # time and services.ceph can only generate units from a static list. The
    # template is declarative; only the instance is dynamic. local-api starts
    # and stops yolab-ceph-osd@<id> from the yolab-disk-config ConfigMap.
    #
    # `start`, never `enable`: enabling writes into /etc/systemd/system, a
    # read-only Nix store path, so it fails outright — observed with both OSDs
    # created and neither started. Boot persistence comes from
    # yolab-ceph-osd-activate below.
    systemd.services."yolab-ceph-osd@" = {
      description = "Ceph OSD %i";
      after = ["network-online.target" "ceph-mon-${host}.service"];
      wants = ["network-online.target"];
      requires = ["ceph-mon-${host}.service"];
      # No wantedBy: enablement is per-instance and owned by local-api.
      #
      # restartIfChanged = false because the default cycles EVERY OSD on the
      # node at once on any unit change — above all a Ceph version bump, which
      # rewrites ExecStart, in the middle of an unattended auto-update. Ceph
      # wants mon -> mgr -> osd one at a time with health checks between;
      # restarting a node's OSDs while another node backfills can drop PGs
      # below min_size and block I/O cluster-wide.
      #
      # A new Ceph build therefore reaches OSDs on the next reboot, not as a
      # side effect of a rebuild. Mixed daemon versions within a release line
      # are explicitly supported, which is what makes that safe.
      restartIfChanged = false;
      path = with pkgs; [ceph ceph-client lvm2 util-linux coreutils];
      # systemd's default (5 starts per 10s, then permanent failure) is the
      # wrong shape for a disk: the reasons an OSD cannot start — cluster down,
      # peer rebooting, machine still coming up — are transient and outlast any
      # burst budget. An OSD that stopped trying looks exactly like a dead one,
      # and nothing else brings it back.
      #
      # 30s rather than 10s because each attempt runs ceph-volume, and a hot
      # loop around it is how one stalled device became twenty-one stuck
      # processes.
      startLimitIntervalSec = 0; # never rate-limit
      serviceConfig = {
        Type = "simple";
        Restart = "on-failure";
        RestartSec = "30s";
        # Bound the whole start, ExecStartPre included. Without it a wedged
        # ceph-volume holds the unit in `activating` for the 90s default, and
        # every one of those attempts leaves its own debris behind.
        TimeoutStartSec = "180s";
        # Primes /var/lib/ceph/osd/ceph-%i from the LVM tags ceph-volume wrote.
        #
        # `activate` takes TWO positionals — {ID} {FSID} — and has no --osd-id
        # flag (checked against the shipped binary, not the docs). Passing the id
        # alone fails, so the OSD's own fsid is read back out of ceph-volume's
        # metadata first. That metadata lives in LVM tags, so this works with no
        # mon reachable, which matters because this runs during boot.
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

    # ── Boot persistence for OSDs ────────────────────────────────────────────
    # Replaces `systemctl enable`, which cannot work here: enabling writes into
    # /etc/systemd/system, a read-only Nix store path. So instead of each OSD
    # instance remembering that it should run, this asks ceph-volume at boot
    # which OSDs are prepared on this host and starts one template instance per
    # OSD. ceph-volume reads LVM tags, so it needs no mon and is correct even
    # when the cluster is unreachable.
    systemd.services.yolab-ceph-osd-activate = {
      description = "Start a yolab-ceph-osd@ instance for every OSD prepared on this host";
      wantedBy = ["multi-user.target"];
      after = ["ceph-mon-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        # NOT RemainAfterExit, deliberately.
        #
        # With it, this unit stays `active (exited)` after boot and never runs
        # again — so if anything stops an OSD later, nothing declarative brings
        # it back until the next reboot. That is not hypothetical: a
        # nixos-rebuild stopped osd.2 on node3 ("Deactivated successfully", a
        # clean stop, so Restart=on-failure does not apply) and it stayed down.
        #
        # Without it the unit ends `inactive`, so switch-to-configuration starts
        # it again on every rebuild, and the timer below re-asserts it
        # periodically. Every step it takes is `systemctl start` on an already
        # running unit, which is a no-op, so running it often is free.
        TimeoutStartSec = "600s";
        ExecStart = "${localApiEnv}/bin/local-api storage osd-activate";
      };
      path = with pkgs; [ceph ceph-client lvm2 util-linux coreutils systemd];
    };

    # Re-assert the OSDs periodically as well as at boot. local-api's reconciler
    # does the same thing and is usually first, but it needs Kubernetes and the
    # disk map to decide anything; this needs neither, so it keeps working in
    # exactly the situations that stop the reconciler.
    systemd.timers.yolab-ceph-osd-activate = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "3min";
        OnUnitActiveSec = "5min";
      };
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
