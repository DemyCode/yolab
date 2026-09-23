{
  config,
  lib,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.imagesStore;
  cephCfg = config.yolab.ceph;
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
  };
}
