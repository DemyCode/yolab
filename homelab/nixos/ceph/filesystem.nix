{
  config,
  lib,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.filesystem;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;
in {
  options.yolab.ceph.filesystem = {
    enable = mkEnableOption "CephFS for PVC storage";
  };

  config = mkIf (cephCfg.enable && cfg.enable) {
    services.ceph.mds = {
      enable = true;
      daemons = [host];
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/ceph/mds 0750 ceph ceph -"
    ];
  };
}
