# containerd's image store, backed by Ceph RBD.
#
# The whole point of moving Ceph out of Kubernetes: host daemons let a node map
# an RBD and mount it as containerd's data-root *before* containerd starts, so
# adding a disk grows the space for images and not just for PVC data.
#
# Two properties are load-bearing — do not "simplify" either:
#
# 1. The RBD is sized against USABLE capacity, not raw. The pool follows the
#    cluster's replica policy like any other, so a logical MB costs `size` raw
#    MB. It was pinned at one copy to avoid paying that, which meant losing one
#    disk took the whole container store with it and the node could not start.
#
# 2. The RBD tracks capacity that really exists, never oversubscribed. Kubelet's
#    image GC works by statfs, so a thin 2TB image over a 500GB pool reports 5%
#    full forever, never collects, and the pool silently reaches full-ratio — at
#    which point Ceph blocks writes for every app on every node. The equivalent
#    failure today is one node's root filling with ENOSPC and GC recovering.
{
  config,
  lib,
  pkgs,
  localApiEnv,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.imagesStore;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;

  # Every unit here runs before k3s, so it can only use host binaries — which is
  # exactly why Ceph had to leave Kubernetes in the first place.
  cephPath = with pkgs; [
    ceph
    ceph-client
    xfsprogs
    e2fsprogs
    util-linux
    coreutils
    systemd
  ];

  # homelab/local-api/src/storage/{images_rbd,containerd_store,images_grow}.rs
  # read these; kept as one set so the three subcommands can never disagree
  # about which pool or filesystem they mean.
  imagesStoreEnv = {
    YOLAB_CEPH_IMAGES_POOL = cfg.poolName;
    YOLAB_CEPH_IMAGES_SHARE = toString cfg.shareOfPool;
    YOLAB_CEPH_IMAGES_MIN_GB = toString cfg.minSizeGb;
    YOLAB_CEPH_IMAGES_FS = cfg.filesystem;
  };
in {
  options.yolab.ceph.imagesStore = {
    enable = mkEnableOption "back containerd's image store with a Ceph RBD";

    poolName = mkOption {
      type = types.str;
      default = "images";
    };

    # What fraction of the pool's free space this node's image store may claim.
    # With N nodes sharing one pool you cannot promise each of them the whole
    # thing: 3 x "500G available" against 500G means all three believe they have
    # room, all three fill, and the pool hits full-ratio anyway. This is the one
    # place a cursor survives — but it is live-adjustable, unlike an LVM split
    # frozen at install time.
    shareOfPool = mkOption {
      type = types.float;
      default = 0.25;
      description = "Fraction of total pool capacity this node's image RBD may claim.";
    };

    minSizeGb = mkOption {
      type = types.int;
      default = 40;
      description = "Never size the image below this — it must always hold the base system's images.";
    };

    filesystem = mkOption {
      type = types.enum ["xfs" "ext4"];
      default = "xfs";
      description = "xfs matches what containerd expects and what most k8s distros use.";
    };
  };

  config = mkIf (cephCfg.enable && cfg.enable) {
    # ── LVM must never scan an RBD ───────────────────────────────────────────
    #
    # Every LVM command reads every block device looking for PV labels,
    # including this node's /dev/rbd0. Ceph blocks rather than fails a read it
    # cannot serve and krbd retries forever, so scanning a stalled RBD parks
    # `lvs` in uninterruptible sleep, where SIGKILL is ignored and the
    # leftovers stay in the unit's cgroup.
    #
    # Observed on node1: eight leaked `lvs`, yolab-local-api unstoppable, and
    # the nixos-rebuild trying to stop it wedged for 17 minutes.
    #
    # It is circular, not merely slow: ceph-volume runs lvs to create an OSD,
    # that OSD is what would let the cluster serve I/O again, and the cluster
    # not serving I/O is what stalls the RBD lvs is blocked on.
    #
    # No OSD ever lives on an RBD, so LVM has no reason to read one.
    # global_filter rather than filter because only the former covers every
    # command including udev-triggered scans, which is where this bites.
    # Flat `section/key` form to match how the upstream NixOS module
    # contributes its own settings.
    environment.etc."lvm/lvm.conf".text = lib.mkAfter ''
      devices/global_filter = [ "r|^/dev/rbd[0-9]+|", "a|.*|" ]
    '';

    # ── Provision the pool and this node's image ─────────────────────────────
    systemd.services.yolab-images-rbd = {
      description = "Ensure the Ceph images pool and this node's RBD image exist";
      wantedBy = ["multi-user.target"];
      # No dependency on any OSD unit: OSD instances are enabled dynamically by
      # local-api, so there is no single unit to order against. The script waits
      # for an OSD to actually report `up` instead, which is the real condition.
      after = ["ceph-mon-${host}.service" "ceph-mgr-${host}.service"];
      requires = ["ceph-mon-${host}.service"];
      # Deliberately NOT RemainAfterExit: on a fresh cluster this runs before any
      # OSD exists and exits cleanly with nothing to do. It has to be able to run
      # again once the user switches a disk on, so the timer below re-runs it.
      serviceConfig = {
        Type = "oneshot";
        # `Type=oneshot` DISABLES the start timeout by default — it is the one
        # service type systemd does not bound. Every unit in this directory was
        # therefore able to hang forever, and on node3 one did: `rbd ls` blocked
        # on an OSD read that could never complete, and k3s (ordered behind it)
        # never started at all. Nothing was broken on that machine; it was
        # waiting on a disk in another one.
        TimeoutStartSec = "300s";
        ExecStart = "${localApiEnv}/bin/local-api storage images-rbd";
      };
      # The reachability/OSD-up waits, the pool-create-if-missing check and the
      # sizing arithmetic now live in
      # homelab/local-api/src/storage/{images_rbd,images_sizing}.rs, with unit
      # tests pinning the same three cases the old images-sizing.nix check drove
      # against the shell version.
      path = with pkgs; [ceph ceph-client];
      environment = imagesStoreEnv;
    };

    # ── Map + mount it, before containerd can start ──────────────────────────
    systemd.services.yolab-containerd-store = {
      description = "Map the images RBD and mount it as containerd's data-root";
      wantedBy = ["multi-user.target"];
      # `after` only, never `requires`, on both sides. This unit must not fail
      # when Ceph has no OSDs yet, and k3s must not fail when this one does —
      # otherwise a fresh cluster deadlocks: k3s waits on the RBD, the RBD waits
      # on an OSD, and OSDs are created by the reconciler from ConfigMaps that
      # only exist once k3s is up. Failing open costs one boot cycle before
      # images move off the root disk; failing closed costs the whole node, with
      # no UI left to diagnose it from.
      after = ["yolab-images-rbd.service"];
      before = ["k3s.service"];
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # Never let this unit's failure propagate into k3s.
        SuccessExitStatus = "0 1";
        # Generous ON PURPOSE. The bounded checks that make FAILURE fast — an
        # unreachable cluster giving up in 20-30s rather than holding k3s —
        # are inside storage::containerd_store now; this is only a last-resort
        # backstop, and it has to be generous because the legitimate slow path
        # (the first migration onto the RBD, possibly gigabytes across the
        # network) must not be mistaken for a hang and killed mid-copy.
        TimeoutStartSec = "900s";
        ExecStart = "${localApiEnv}/bin/local-api storage containerd-store";
      };
      # The mount/readability check that replaced a bare `mountpoint` (see this
      # file's header — the seventeen-hour incident), the k3s stop/start
      # bracket, and the migrate-then-swap dance all live in
      # homelab/local-api/src/storage/containerd_store.rs now, with unit tests
      # covering the migration end to end and the k3s restart ordering.
      path = cephPath;
      environment = imagesStoreEnv;
    };

    # Ordered after the store unit so the mount lands before containerd opens
    # its data-root — but with `after` only, never `requires`. A hard dependency
    # here is what deadlocks a fresh cluster (see the note on that unit).
    systemd.services.k3s.after = ["yolab-containerd-store.service"];

    # ── Growth ───────────────────────────────────────────────────────────────
    # Without this the whole feature is inert: you would add a disk, the pool
    # would grow, and the image store would stay exactly the same size forever.
    systemd.services.yolab-images-rbd-grow = {
      description = "Grow the images RBD as the Ceph pool grows";
      # `requires`, not just `after`: the timer fires on a schedule and would
      # otherwise run during bootstrap, before the admin keyring exists, and
      # fail noisily with "unable to find a keyring" on a cluster that is
      # perfectly healthy — observed on the first live switch.
      after = ["yolab-containerd-store.service"];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "300s";
        ExecStart = "${localApiEnv}/bin/local-api storage images-grow";
      };
      # The not-ready checks, the current-vs-wanted size comparison and the
      # grow-only guard (never shrink a mounted filesystem) now live in
      # homelab/local-api/src/storage/images_grow.rs.
      path = cephPath;
      environment = imagesStoreEnv;
    };

    # Re-runs provisioning so the pool and image appear shortly after the first
    # disk is switched on, without needing a reboot to get there.
    systemd.timers.yolab-images-rbd = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "2min";
        OnUnitActiveSec = "5min";
      };
    };

    # The other half of that. Provisioning creating the RBD achieved nothing on
    # its own: the unit that mounts it only ran at boot, so a first install left
    # containerd on the root disk until somebody rebooted — indefinitely, and
    # invisibly. Runs behind the provisioning timer so the image exists by the
    # time it looks, and exits immediately once the mount is in place.
    systemd.timers.yolab-containerd-store = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "4min";
        OnUnitActiveSec = "5min";
      };
    };

    systemd.timers.yolab-images-rbd-grow = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "10min";
        OnUnitActiveSec = "1h";
      };
    };
  };
}
