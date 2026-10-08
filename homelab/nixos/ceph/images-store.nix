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
  releaseUnit = "yolab-image-store.service";
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

    systemd.services.yolab-image-store = {
      description = "Release containerd's image store before Ceph and the network stop";
      after = [
        "network.target"
        "network-online.target"
        "wireguard-wg0.service"
        "wireguard-wg1.service"
        "ceph-mon-${host}.service"
      ];
      restartIfChanged = false;
      stopIfChanged = false;
      serviceConfig = {
        Type = "oneshot";
        RemainAfterExit = true;
        TimeoutStopSec = "900";
        ExecStart = "${pkgs.coreutils}/bin/true";
        ExecStop = "${localApiEnv}/bin/local-api storage release-images";
      };
      path = with pkgs; [
        util-linux
        procps
        systemd
        coreutils
      ];
    };

    systemd.services."yolab-ceph-osd@".before = [releaseUnit];
    systemd.services.k3s.after = [releaseUnit];
    systemd.services.yolab-local-api.after = [releaseUnit];
  };
}
