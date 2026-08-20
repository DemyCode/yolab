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
{
  config,
  lib,
  pkgs,
  ...
}:
with lib; let
  cfg = config.yolab.ceph;
  host = config.networking.hostName;
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

    monInitialMembers = mkOption {
      type = types.listOf types.str;
      default = [host];
    };

    isBootstrapNode = mkOption {
      type = types.bool;
      default = true;
      description = ''
        True on the node that creates the cluster. Joining nodes fetch keyrings
        and the monmap from an existing mon rather than generating their own.
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
        monHost = "[v2:[${cfg.monAddr}]:3300,v1:[${cfg.monAddr}]:6789]";
        # /128 because the cluster network is a WireGuard mesh of individual
        # addresses, not a broadcast subnet.
        publicNetwork = "${cfg.monAddr}/128";
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

        # Cap BlueStore's cache so an OSD is never OOM-killed mid-write; a
        # SIGKILL can leave the BlueStore label half-written and unrecoverable.
        bluestore_cache_size_ssd = "1073741824";
        osd_max_backfills = "4";
        osd_recovery_max_active = "4";
      };

      mon = {
        enable = true;
        daemons = [host];
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

    # ── Cluster bootstrap ────────────────────────────────────────────────────
    # Idempotent: a reboot is a no-op and a rebuild never re-bootstraps.
    systemd.services.yolab-ceph-bootstrap = {
      description = "Bootstrap the Ceph cluster (keyrings, monmap, mon mkfs)";
      wantedBy = ["multi-user.target"];
      before = ["ceph-mon-${host}.service"];
      requiredBy = ["ceph-mon-${host}.service"];
      after = ["network-online.target" "wireguard-wg1.service"];
      wants = ["network-online.target"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
      };
      path = with pkgs; [ceph ceph-client coreutils];
      script = ''
        set -euo pipefail
        MON_DIR=/var/lib/ceph/mon/ceph-${host}
        if [ -f "$MON_DIR/keyring" ]; then
          echo "already bootstrapped"
          exit 0
        fi

        ${optionalString (!cfg.isBootstrapNode) ''
          echo "not the bootstrap node — joining is handled by local-api at runtime"
          exit 0
        ''}

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
          --addv ${host} '[v2:[${cfg.monAddr}]:3300,v1:[${cfg.monAddr}]:6789]' \
          --fsid ${cfg.fsid} /tmp/monmap

        mkdir -p "$MON_DIR"
        ceph-mon --mkfs -i ${host} --monmap /tmp/monmap --keyring /tmp/ceph.mon.keyring
        cp /tmp/ceph.mon.keyring "$MON_DIR/keyring"
        chown -R ceph:ceph /var/lib/ceph
        chown ceph:ceph /etc/ceph/ceph.client.admin.keyring
        rm -f /tmp/ceph.mon.keyring /tmp/monmap
      '';
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
      };
      path = with pkgs; [ceph ceph-client coreutils];
      script = ''
        set -euo pipefail
        MGR_DIR=/var/lib/ceph/mgr/ceph-${host}
        [ -f "$MGR_DIR/keyring" ] && exit 0
        mkdir -p "$MGR_DIR"
        for _ in $(seq 1 60); do
          ceph -s >/dev/null 2>&1 && break
          sleep 1
        done
        ceph auth get-or-create mgr.${host} \
          mon 'allow profile mgr' osd 'allow *' mds 'allow *' \
          -o "$MGR_DIR/keyring"
        chown -R ceph:ceph "$MGR_DIR"
      '';
    };

    # ── OSD daemons ──────────────────────────────────────────────────────────
    # A systemd *template* unit, which is what makes dynamic OSD ids work at
    # all. services.ceph can only generate units for a statically declared
    # `osd.daemons` list, but Ceph assigns an OSD its id at creation time — so
    # the id can never be known when the config is built.
    #
    # The template is declarative; only the instance is dynamic. local-api runs
    # `systemctl enable --now yolab-ceph-osd@<id>` when a disk is switched ON
    # and `disable --now` when it is switched OFF, driven by the shared
    # yolab-disk-config ConfigMap. Because instances are enabled (not transient),
    # they come back on their own after a reboot.
    systemd.services."yolab-ceph-osd@" = {
      description = "Ceph OSD %i";
      after = ["network-online.target" "ceph-mon-${host}.service"];
      wants = ["network-online.target"];
      requires = ["ceph-mon-${host}.service"];
      # No wantedBy here: enablement is per-instance and owned by local-api.
      path = with pkgs; [ceph ceph-client lvm2 util-linux coreutils];
      serviceConfig = {
        Type = "simple";
        Restart = "on-failure";
        RestartSec = "10s";
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
          FSID=$(ceph-volume lvm list "$OSD_ID" --format json 2>/dev/null \
            | jq -r --arg id "$OSD_ID" '.[$id][0].tags["ceph.osd_fsid"] // empty')
          if [ -z "$FSID" ]; then
            echo "osd.$OSD_ID: no ceph-volume metadata found for it on this host" >&2
            exit 1
          fi
          # --no-systemd stops ceph-volume generating its own competing units;
          # this template is the single supervisor for every OSD on the host.
          exec ceph-volume lvm activate --no-systemd "$OSD_ID" "$FSID"
        ''
        + " %i";
        ExecStart = "${pkgs.ceph}/bin/ceph-osd -f -i %i --setuser ceph --setgroup ceph";
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
