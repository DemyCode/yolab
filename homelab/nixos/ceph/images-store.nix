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

  cephPath = with pkgs; [
    ceph
    ceph-client
    xfsprogs
    e2fsprogs
    util-linux
    coreutils
    systemd
  ];

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
    environment.etc."lvm/lvm.conf".text = lib.mkAfter ''
      devices/global_filter = [ "r|^/dev/rbd|", "r|^/dev/block/|", "r|^/dev/disk/|", "a|.*|" ]
    '';

    systemd.services.yolab-images-rbd = {
      description = "Ensure the Ceph images pool and this node's RBD image exist";
      wantedBy = ["multi-user.target"];
      after = ["yolab-ceph-system-osd.service" "ceph-mon-${host}.service" "ceph-mgr-${host}.service"];
      wants = ["yolab-ceph-system-osd.service" "ceph-mon-${host}.service"];
      restartIfChanged = false;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        # NOT infinity: images_rbd::attempt returns NotYet forever (no
        # give-up) while stat.num_up_osds == 0, which is exactly the state
        # every node boots into before any OSD exists — first boot on a
        # freshly wiped machine, or any boot before a disk has been switched
        # on. yolab-containerd-store is After=/Wants= this unit, and k3s is
        # After=/Wants=/Before=-ed to containerd-store in turn (see
        # containerd-store-after-order in nix/checks.nix), so a start job
        # that never reaches a terminal state here holds up k3s — and
        # multi-user.target with it — exactly like yolab-ceph-system-osd did
        # (see the comment there).
        #
        # 120s, matching system-osd's own bound and for the same reason: this
        # is the middle link of a three-deep chain, and 600s here compounded
        # with 600s on either side of it into a 30-minute worst case that
        # blew past disk-loss-test's 900s budget.
        TimeoutStartSec = "120s";
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
        # NOT infinity, same reasoning as yolab-images-rbd just above: this
        # unit is explicitly Before=k3s.service, so this is the most direct
        # of the three links in the chain — if it never reaches a terminal
        # state, k3s never even gets a start job queued. Bounding it lets k3s
        # proceed once it gives up, using whatever is already mounted at
        # containerd's data-root (the root filesystem, by default) — which is
        # the actual mechanism behind "k3s must come up whether or not there
        # is an OSD yet" in nix/tests/boot.nix's own comment. containerd_store
        # ::attempt has no explicit fallback branch; this bound is what makes
        # that comment true rather than aspirational.
        #
        # 120s, matching its two predecessors in the chain — see
        # yolab-ceph-system-osd's comment for why 600s here compounded into a
        # 30-minute worst case.
        TimeoutStartSec = "120s";
        ExecStart = "${localApiEnv}/bin/local-api storage containerd-store";
      };
      path = cephPath;
      environment = imagesStoreEnv;
    };

    systemd.services.k3s = {
      after = ["yolab-containerd-store.service"];
      wants = ["yolab-containerd-store.service"];
    };
  };
}
