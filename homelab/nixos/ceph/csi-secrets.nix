# Publish the host Ceph cluster's credentials into Kubernetes for ceph-csi.
#
# Rook in external mode does not discover anything: it reads a fixed set of
# Secrets and one ConfigMap and hands their contents to the CSI drivers. Rook's
# upstream `import-external-cluster.sh` is what normally creates them, from a
# human running a script against the Ceph cluster. This does the same thing as a
# reconciling unit, so it survives reboots, re-runs after a mon address change,
# and needs no operator intervention.
#
# It runs *after* k3s, unlike everything else in this directory — it is the one
# piece of the storage stack that legitimately depends on Kubernetes, because
# its whole job is writing Kubernetes objects.
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
        # `Type=oneshot` disables the start timeout by default; see the note on
        # yolab-ceph-bootstrap in default.nix.
        TimeoutStartSec = "180s";
        ExecStart = "${localApiEnv}/bin/local-api storage csi-secrets";
      };
      environment.KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      # The mon-dump parsing (why v1, not v2 — see that module's header), the
      # cephx caps and the wrong-Secret-type recreation dance (Secret types
      # are immutable, so an Opaque one Rook cannot update has to be deleted
      # first) all live in homelab/local-api/src/storage/csi_secrets.rs now.
      path = with pkgs; [ceph ceph-client k3s];
    };

    # Re-runs so credentials appear once both Ceph and k3s are up, and so a mon
    # address change propagates without a reboot.
    systemd.timers.yolab-ceph-csi-secrets = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "4min";
        OnUnitActiveSec = "10min";
        # A failed attempt never reaches the active state OnUnitActiveSec
        # measures from — see the note on yolab-ceph-mgr-key's timer.
        OnUnitInactiveSec = "2min";
      };
    };
  };
}
