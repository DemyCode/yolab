{config, ...}: let
  host = config.networking.hostName;
in {
  config = {
    services.ceph.mds = {
      enable = true;
      daemons = [host];
    };

    systemd.tmpfiles.rules = [
      "d /var/lib/ceph/mds 0750 ceph ceph -"
    ];
  };
}
