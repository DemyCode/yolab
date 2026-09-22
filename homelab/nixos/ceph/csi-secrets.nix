{
  config,
  lib,
  pkgs,
  localApiEnv,
  ...
}:
with lib; let
  cfg = config.yolab.ceph.csiSecrets;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;
in {
  options.yolab.ceph.csiSecrets.enable =
    mkEnableOption "publish host Ceph credentials to Kubernetes for ceph-csi";

  config = mkIf (cephCfg.enable && cfg.enable) {
    systemd.services.yolab-ceph-csi-secrets = {
      description = "Publish host Ceph credentials into Kubernetes for ceph-csi";
      wantedBy = ["multi-user.target"];
      after = ["k3s.service" "ceph-mon-${host}.service"];
      serviceConfig = {
        Type = "oneshot";
        TimeoutStartSec = "180s";
        ExecStart = "${localApiEnv}/bin/local-api storage csi-secrets";
      };
      environment.KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      path = with pkgs; [ceph ceph-client k3s];
    };

  };
}
