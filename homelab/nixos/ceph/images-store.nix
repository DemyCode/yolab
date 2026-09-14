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

    # The guard against mistaking a blip for a lost disk. `down` means no copy is
    # available RIGHT NOW, not that one is gone: a nixos-rebuild takes an OSD's LV
    # down for ~90s and a node reboot for a few minutes, and both recover on their
    # own. Rebuilding on those would cost every node its image cache and force a
    # simultaneous re-pull across the uplink — worse than the outage being fixed.
    # 15 minutes clears both with room to spare while still being far short of the
    # 23 hours the cluster sat waiting on 2026-09-10.
    recoverGraceSeconds = mkOption {
      type = types.int;
      default = 900;
      description = ''
        How long the images pool must stay unable to serve reads before its
        unrecoverable placement groups are rebuilt empty. Only ever applies to the
        images pool, whose every object is a container layer a registry will send
        again — never to the pools holding the owner's data.
      '';
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

    # ── The boot line to k3s ─────────────────────────────────────────────────
    #
    #   yolab-ceph-system-osd → yolab-images-rbd → yolab-containerd-store → k3s
    #
    # One path, with no fallback. Each step WAITS for what it needs (the reason
    # is in its journal while it does) instead of exiting with nothing done, so
    # k3s only ever starts with containerd's data-root on this node's RBD, and
    # nothing later has to move the store underneath a running k3s. That move —
    # a controller stopping k3s every five minutes — restarted k3s 73 times on
    # node1 in one day and orphaned every container it had been running
    # (KillMode=process). See homelab/local-api/src/storage/containerd_store.rs.
    #
    # All three are once-per-boot: RemainAfterExit, and restartIfChanged = false
    # so a rebuild never re-runs them under a node that is already up.
    # `TimeoutStartSec = "infinity"` because waiting IS their job; every command
    # inside is individually bounded, so what waits is the loop, never a hung
    # process.

    systemd.services.yolab-images-rbd = {
      description = "Ensure the Ceph images pool and this node's RBD image exist";
      wantedBy = ["multi-user.target"];
      after = ["yolab-ceph-system-osd.service" "ceph-mon-${host}.service" "ceph-mgr-${host}.service"];
      wants = ["yolab-ceph-system-osd.service"];
      requires = ["ceph-mon-${host}.service"];
      restartIfChanged = false;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "infinity";
        ExecStart = "${localApiEnv}/bin/local-api storage images-rbd";
      };
      path = with pkgs; [ceph ceph-client];
      environment = imagesStoreEnv;
    };

    systemd.services.yolab-containerd-store = {
      description = "Mount this node's images RBD as containerd's data-root";
      wantedBy = ["multi-user.target"];
      after = ["yolab-images-rbd.service"];
      wants = ["yolab-images-rbd.service"];
      before = ["k3s.service"];
      restartIfChanged = false;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStartSec = "infinity";
        ExecStart = "${localApiEnv}/bin/local-api storage containerd-store";
      };
      path = cephPath;
      environment = imagesStoreEnv;
    };

    # k3s starts only once the store is in place. `wants`, not `requires`: a
    # Requires= would also STOP k3s whenever this oneshot is stopped, and the
    # store step never fails — it waits — so there is no failure to propagate.
    systemd.services.k3s = {
      after = ["yolab-containerd-store.service"];
      wants = ["yolab-containerd-store.service"];
    };

    # ── Growth ───────────────────────────────────────────────────────────────
    # Without this the whole feature is inert: you would add a disk, the pool
    # would grow, and the image store would stay exactly the same size forever.
    # Re-run by the `images-grow` controller; this unit is for running it by hand.
    systemd.services.yolab-images-rbd-grow = {
      description = "Grow the images RBD as the Ceph pool grows";
      after = ["yolab-containerd-store.service"];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "300s";
        ExecStart = "${localApiEnv}/bin/local-api storage images-grow";
      };
      path = cephPath;
      environment = imagesStoreEnv;
    };
  };
}
