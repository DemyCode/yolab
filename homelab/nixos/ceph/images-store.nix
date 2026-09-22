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

    systemd.services.k3s = {
      after = ["yolab-containerd-store.service"];
      wants = ["yolab-containerd-store.service"];
    };

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
