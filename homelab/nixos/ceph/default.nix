# Ceph as host-level systemd daemons, outside Kubernetes.
#
# WHY THIS EXISTS — the constraint that forced it
# ------------------------------------------------
# The goal is that adding a disk grows the space available for *container
# images*, not just for PVC data. Images are per-node and disposable, so the
# only way to make cluster storage back them is to put containerd's data-root
# on a Ceph RBD.
#
# That is impossible while Ceph runs inside Kubernetes. Mapping an RBD needs a
# mon; under Rook the mon is a *pod*; a pod needs containerd; containerd needs
# the RBD. On a single-node cluster there is no other machine to break the
# cycle, so it is a hard deadlock, not a race — and no ordering trick escapes
# it, because containerd has exactly one image store. A "seed cache" on the root
# disk does not help: mounting the RBD over the data-root hides the seed, and
# leaving it local means nothing moved.
#
# Ceph as host daemons breaks the cycle: mon/mgr/osd are plain binaries that
# need no container runtime at all, so they are up long before containerd.
#
# WHAT services.ceph DOES AND DOES NOT DO
# ---------------------------------------
# nixos/modules/services/network-filesystems/ceph.nix is daemon *supervision*
# only — it generates systemd units from static daemon-id lists and writes
# ceph.conf. It contains no ceph-volume, no --mkfs, and no keyring generation.
# So:
#   - mon/mgr ids are the hostname, known at build time -> declared here.
#   - cluster bootstrap (keyrings, monmap, mon --mkfs) -> yolab-ceph-bootstrap.
#   - OSDs get ids allocated by Ceph at creation time, so they can never be a
#     static list. Creation is driven at runtime by local-api from the
#     yolab-disk-config ConfigMap (unchanged UI contract: the disk ON/OFF
#     toggle), and this module only re-activates already-prepared OSDs at boot.
#
# EVERY NODE IS A PEER
# --------------------
# There is no master. Every machine runs its own mon, mgr, MDS and OSDs, and
# the only thing the first machine does differently is *create* the cluster —
# a one-time event, not a role it keeps. After that it is interchangeable with
# any other node, and losing it costs no more than losing any other.
#
# The cost is stated plainly because it cannot be designed away: mons agree by
# majority, so N mons tolerate (N-1)/2 failures. Two machines means two mons
# means both must be up. That is deliberately accepted here — two machines was
# never going to survive one of them dying anyway (two copies across two hosts
# leaves you at min_size), and three machines is where it starts to.
#
# IF THE CLUSTER LOSES QUORUM
# ---------------------------
# Symptom: every `ceph` command hangs, and `ceph -s --connect-timeout 5` times
# out on every node. It means fewer than a majority of the mons in the monmap
# are running. If a machine is merely off, turn it back on — that is the whole
# fix. If it is gone for good, edit the monmap by hand on a surviving node:
#
#   systemctl stop ceph-mon-$(hostname)
#   ceph-mon -i $(hostname) --extract-monmap /tmp/monmap
#   monmaptool /tmp/monmap --rm <name-of-the-dead-mon>
#   ceph-mon -i $(hostname) --inject-monmap /tmp/monmap
#   systemctl start ceph-mon-$(hostname)
#
# Nothing automates that on purpose: a node that is offline is indistinguishable
# from one that has left, so an automatic version would let a rebooting machine
# be evicted by its peer.
{
  config,
  lib,
  pkgs,
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
        mon_warn_on_pool_no_redundancy = "false";

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
      };
      path = with pkgs; [ceph ceph-client coreutils curl jq gnugrep systemd];
      # At boot this is redundant — systemd already orders ceph-mon after us.
      # It matters on a *retry*: when the first attempt failed, the mon's start
      # job was dropped with it, so nothing would bring the daemon up when a
      # later attempt finally succeeds. --no-block because the mon must not be
      # waited on from inside a unit it is ordered after.
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mon-${host}.service || true
      '';
      script = ''
        set -euo pipefail
        MON_DIR=/var/lib/ceph/mon/ceph-${host}
        if [ -f "$MON_DIR/keyring" ]; then
          echo "already a member of this cluster"
          exit 0
        fi

        ${
          if isBootstrap
          then ''
            # ── Create ────────────────────────────────────────────────────────
            ceph-authtool --create-keyring /tmp/ceph.mon.keyring \
              --gen-key -n mon. --cap mon 'allow *'
            ceph-authtool --create-keyring /etc/ceph/ceph.client.admin.keyring \
              --gen-key -n client.admin \
              --cap mon 'allow *' --cap osd 'allow *' --cap mds 'allow *' --cap mgr 'allow *'
            ceph-authtool --create-keyring /var/lib/ceph/bootstrap-osd/ceph.keyring \
              --gen-key -n client.bootstrap-osd \
              --cap mon 'profile bootstrap-osd' --cap mgr 'allow r'
            ceph-authtool /tmp/ceph.mon.keyring --import-keyring /etc/ceph/ceph.client.admin.keyring
            ceph-authtool /tmp/ceph.mon.keyring --import-keyring /var/lib/ceph/bootstrap-osd/ceph.keyring

            monmaptool --create \
              --addv ${host} '${addrvec cfg.monAddr}' \
              --fsid ${cfg.fsid} /tmp/monmap

            mkdir -p "$MON_DIR"
          ''
          else ''
            # ── Join ──────────────────────────────────────────────────────────
            # Same WireGuard mesh and same shared secret as the k3s join: the
            # account_token in config.toml is what already authorizes a machine
            # to become a control-plane peer, so it authorizes this too. It is a
            # separate endpoint from /api/cluster/join-info only because that one
            # must keep answering when Ceph is down — otherwise a storage fault
            # on this machine would stop a new node joining Kubernetes at all.
            SEED='${cfg.joinSeedAddr}'
            CONFIG="${config.yolab.repoPath}/homelab/ignored/config.toml"

            TOKEN=$(grep -oP 'account_token\s*=\s*"\K[^"]+' "$CONFIG" 2>/dev/null | head -n1 || true)
            if [ -z "$TOKEN" ]; then
              echo "no tunnel.account_token in $CONFIG — cannot authenticate to [$SEED]" >&2
              exit 1
            fi

            if ! BUNDLE=$(curl -fsS --max-time 20 \
                  -H "x-yolab-cluster: $TOKEN" \
                  "http://[$SEED]:3001/api/cluster/ceph-join"); then
              echo "[$SEED] did not hand over the cluster credentials; retrying on the timer" >&2
              exit 1
            fi

            # The one check that must never be skipped. The installer copies the
            # fsid across with the k3s token, and a mismatch means this machine
            # is about to mkfs a mon for a DIFFERENT cluster — which does not
            # fail loudly, it quietly produces a second isolated cluster that
            # looks fine until someone wonders where their data went.
            SEED_FSID=$(printf '%s' "$BUNDLE" | jq -r '.fsid // ""')
            if [ "$SEED_FSID" != '${cfg.fsid}' ]; then
              echo "refusing to join: [$SEED] runs cluster $SEED_FSID, this node is configured for ${cfg.fsid}" >&2
              exit 1
            fi

            umask 077
            printf '%s' "$BUNDLE" | jq -er '.admin_keyring' > /etc/ceph/ceph.client.admin.keyring
            printf '%s' "$BUNDLE" | jq -er '.bootstrap_osd_keyring' > /var/lib/ceph/bootstrap-osd/ceph.keyring
            printf '%s' "$BUNDLE" | jq -er '.mon_keyring' > /tmp/ceph.mon.keyring
            chown ceph:ceph /etc/ceph/ceph.client.admin.keyring /var/lib/ceph/bootstrap-osd/ceph.keyring

            # Fetched live rather than shipped in the bundle: a copied monmap is
            # a snapshot that is wrong the moment mon membership changes, and
            # with client.admin in place this is one call.
            rm -f /tmp/monmap
            for _ in $(seq 1 30); do
              ceph --connect-timeout 10 mon getmap -o /tmp/monmap >/dev/null 2>&1 && break
              sleep 2
            done
            if [ ! -s /tmp/monmap ]; then
              echo "could not fetch a monmap from the cluster; retrying on the timer" >&2
              exit 1
            fi

            # Only reached when $MON_DIR/keyring is absent, i.e. the store is not
            # one the mon can start from (ConditionPathExists in the upstream
            # unit guarantees the daemon never opened it). Clearing a half-built
            # store is what makes a retry after an interrupted join work at all:
            # ceph-mon --mkfs refuses to write into a non-empty directory.
            rm -rf "$MON_DIR"
            mkdir -p "$MON_DIR"
          ''
        }

        ceph-mon --mkfs -i ${host} --monmap /tmp/monmap --keyring /tmp/ceph.mon.keyring
        cp /tmp/ceph.mon.keyring "$MON_DIR/keyring"
        chown -R ceph:ceph /var/lib/ceph
        chown ceph:ceph /etc/ceph/ceph.client.admin.keyring
        rm -f /tmp/ceph.mon.keyring /tmp/monmap
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
      };
      path = with pkgs; [ceph ceph-client coreutils jq systemd];
      script = ''
        set -uo pipefail
        MON_DIR=/var/lib/ceph/mon/ceph-${host}
        if [ ! -f "$MON_DIR/keyring" ]; then
          echo "this node has not joined yet — yolab-ceph-bootstrap runs first"
          exit 0
        fi

        if ! systemctl is-active --quiet ceph-mon-${host}.service; then
          systemctl start --no-block ceph-mon-${host}.service
          echo "started the local mon; membership is checked on the next run"
          exit 0
        fi

        in_monmap() {
          ceph --connect-timeout 10 mon dump -f json 2>/dev/null \
            | jq -e --arg n '${host}' 'any(.mons[]; .name == $n)' >/dev/null
        }

        # --connect-timeout on every call, tighter than the 30s
        # client_mount_timeout in ceph.conf: this unit runs on a timer, so
        # returning quickly and trying again beats waiting out a full timeout
        # while a peer reboots.
        for _ in $(seq 1 30); do
          ceph --connect-timeout 10 -s >/dev/null 2>&1 && break
          sleep 2
        done
        if ! ceph --connect-timeout 10 -s >/dev/null 2>&1; then
          echo "cluster is not answering — not touching the monmap"
          exit 0
        fi

        if in_monmap; then
          echo "${host} is already in the monmap"
          exit 0
        fi

        echo "adding ${host} to the monmap"
        ceph --connect-timeout 10 mon add ${host} '${addrvec cfg.monAddr}' || true

        for _ in $(seq 1 60); do
          if in_monmap; then
            echo "${host} joined the quorum"
            exit 0
          fi
          sleep 2
        done
        echo "${host} is still not in the monmap — see this module's header for recovery" >&2
      '';
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
      };
      path = with pkgs; [ceph ceph-client coreutils systemd];
      # Same reason as yolab-ceph-bootstrap's: at boot systemd already orders
      # ceph-mgr after this, but if this attempt failed the mgr's start job died
      # with it, and nothing would bring the daemon up when the retry succeeds.
      postStart = ''
        ${pkgs.systemd}/bin/systemctl start --no-block ceph-mgr-${host}.service || true
      '';
      script = ''
        set -euo pipefail
        MGR_DIR=/var/lib/ceph/mgr/ceph-${host}
        [ -f "$MGR_DIR/keyring" ] && exit 0
        mkdir -p "$MGR_DIR"
        for _ in $(seq 1 60); do
          ceph -s >/dev/null 2>&1 && break
          sleep 1
        done
        # Fail rather than press on: only the mon can mint this key, so an
        # unreachable cluster means "not yet", not "no key needed". On a joining
        # node this is the ordinary state until yolab-ceph-bootstrap succeeds,
        # which is why the unit has a retry timer.
        if ! ceph -s >/dev/null 2>&1; then
          echo "cluster not reachable — cannot mint the mgr key yet" >&2
          exit 1
        fi
        ceph auth get-or-create mgr.${host} \
          mon 'allow profile mgr' osd 'allow *' mds 'allow *' \
          -o "$MGR_DIR/keyring"
        chown -R ceph:ceph "$MGR_DIR"
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
    # A systemd *template* unit, which is what makes dynamic OSD ids work at
    # all. services.ceph can only generate units for a statically declared
    # `osd.daemons` list, but Ceph assigns an OSD its id at creation time — so
    # the id can never be known when the config is built.
    #
    # The template is declarative; only the instance is dynamic. local-api runs
    # `systemctl start yolab-ceph-osd@<id>` when a disk is switched ON and
    # `stop` when it is switched OFF, driven by the shared yolab-disk-config
    # ConfigMap.
    #
    # It is `start`, never `enable`: enabling writes a symlink into
    # /etc/systemd/system, which on NixOS is a read-only Nix store path.
    # `systemctl enable` therefore fails outright ("Read-only file system"),
    # observed live with both OSDs created but neither ever started. Boot
    # persistence comes from yolab-ceph-osd-activate below instead, which is the
    # declarative equivalent and the only shape NixOS actually supports.
    systemd.services."yolab-ceph-osd@" = {
      description = "Ceph OSD %i";
      after = ["network-online.target" "ceph-mon-${host}.service"];
      wants = ["network-online.target"];
      requires = ["ceph-mon-${host}.service"];
      # No wantedBy here: enablement is per-instance and owned by local-api.
      #
      # Do NOT let switch-to-configuration restart these. The default
      # (restartIfChanged = true) means any change to the unit — above all a
      # Ceph version bump, which rewrites ExecStart — cycles EVERY OSD on the
      # node simultaneously, in the middle of an unattended auto-update.
      #
      # That is both the riskiest possible moment and the wrong order: Ceph is
      # meant to be upgraded mon → mgr → osd, one daemon at a time, checking
      # health in between. On a replicated cluster, restarting a node's OSDs
      # while another node is still backfilling can drop PGs below min_size and
      # block I/O for every app on every node.
      #
      # So a new Ceph build takes effect for OSDs on the next reboot (or a
      # deliberate `systemctl restart yolab-ceph-osd@N`), not as a side effect
      # of a rebuild. Ceph explicitly supports running mixed daemon versions
      # within a release line, which is exactly what makes that safe. mon/mgr/mds
      # keep the default and restart normally — they are the ones Ceph wants
      # upgraded first anyway.
      restartIfChanged = false;
      path = with pkgs; [ceph ceph-client lvm2 util-linux coreutils];
      # Keep retrying, but slowly, and never give up.
      #
      # systemd's defaults are 5 starts per 10s and then permanent failure,
      # which is the wrong shape for a disk: the reasons an OSD cannot start
      # (the cluster is down, a peer is rebooting, the machine is still coming
      # up) are transient and can outlast any burst budget. An OSD that stopped
      # trying is indistinguishable from a dead disk, and nothing else brings it
      # back — ensure_osd_unit_running only acts on OSDs the reconciler can see,
      # which needs the very ceph-volume metadata this unit is waiting on.
      #
      # 30s between attempts rather than 10s because each failed attempt runs
      # ceph-volume, and a hot loop around it is how one stalled device turned
      # into twenty-one stuck processes.
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
        ExecStartPre = pkgs.writeShellScript "yolab-ceph-osd-activate" ''
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
      };
      path = with pkgs; [ceph ceph-client lvm2 util-linux coreutils jq systemd];
      script = ''
        set -uo pipefail
        IDS=$(timeout 60 ceph-volume lvm list --format json 2>/dev/null | jq -r 'keys[]' 2>/dev/null || true)
        if [ -z "$IDS" ]; then
          echo "no OSDs prepared on this host"
          exit 0
        fi
        for id in $IDS; do
          echo "starting yolab-ceph-osd@$id"
          systemctl start "yolab-ceph-osd@$id.service" || \
            echo "osd.$id: failed to start" >&2
        done
      '';
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
