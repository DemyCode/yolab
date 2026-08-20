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
  ...
}:
with lib; let
  cfg = config.yolab.ceph.csiSecrets;
  cephCfg = config.yolab.ceph;
  host = config.networking.hostName;
  ns = "rook-ceph";
in {
  options.yolab.ceph.csiSecrets.enable =
    mkEnableOption "publish host Ceph credentials to Kubernetes for ceph-csi";

  config = mkIf (cephCfg.enable && cfg.enable) {
    systemd.services.yolab-ceph-csi-secrets = {
      description = "Publish host Ceph credentials into Kubernetes for ceph-csi";
      wantedBy = ["multi-user.target"];
      after = ["k3s.service" "ceph-mon-${host}.service"];
      serviceConfig.Type = "oneshot";
      environment.KUBECONFIG = "/etc/rancher/k3s/k3s.yaml";
      path = with pkgs; [ceph ceph-client k3s coreutils gnugrep jq];
      script = ''
        set -uo pipefail

        # Both sides have to be up. Neither being ready is an ordinary state on
        # a fresh boot, so exit cleanly and let the timer retry.
        if ! ceph -s >/dev/null 2>&1; then
          echo "ceph not reachable yet"
          exit 0
        fi
        if ! kubectl get ns ${ns} >/dev/null 2>&1; then
          echo "kubernetes not reachable yet (or namespace missing)"
          exit 0
        fi

        FSID=$(ceph fsid)
        MON_ADDR="[${cephCfg.monAddr}]:6789"

        # CSI needs restricted users, not client.admin. These caps are the ones
        # Rook's own import script grants; broader caps would hand every CSI pod
        # cluster-admin over Ceph.
        CEPHFS_PROV=$(ceph auth get-or-create-key client.csi-cephfs-provisioner \
          mon 'allow r' mgr 'allow rw' osd 'allow rw tag cephfs metadata=*')
        CEPHFS_NODE=$(ceph auth get-or-create-key client.csi-cephfs-node \
          mon 'allow r' mgr 'allow rw' osd 'allow rw tag cephfs *=*' mds 'allow rw')
        RBD_PROV=$(ceph auth get-or-create-key client.csi-rbd-provisioner \
          mon 'profile rbd' mgr 'allow rw' osd 'profile rbd')
        RBD_NODE=$(ceph auth get-or-create-key client.csi-rbd-node \
          mon 'profile rbd' osd 'profile rbd')

        apply_secret() {
          # `create --dry-run | apply` so this is an upsert: the unit re-runs on
          # every boot and must converge, not fail on "already exists".
          kubectl create secret generic "$1" -n ${ns} \
            --from-literal="$2=$3" --from-literal="$4=$5" \
            --dry-run=client -o yaml | kubectl apply -f - >/dev/null
        }

        apply_secret rook-csi-cephfs-provisioner \
          adminID csi-cephfs-provisioner adminKey "$CEPHFS_PROV"
        apply_secret rook-csi-cephfs-node \
          adminID csi-cephfs-node adminKey "$CEPHFS_NODE"
        apply_secret rook-csi-rbd-provisioner \
          userID csi-rbd-provisioner userKey "$RBD_PROV"
        apply_secret rook-csi-rbd-node \
          userID csi-rbd-node userKey "$RBD_NODE"

        # The operator reads fsid + admin credentials from here.
        ADMIN_KEY=$(ceph auth get-key client.admin)
        kubectl create secret generic rook-ceph-mon -n ${ns} \
          --from-literal=cluster-name=${ns} \
          --from-literal=fsid="$FSID" \
          --from-literal=admin-secret=admin-secret \
          --from-literal=mon-secret=mon-secret \
          --from-literal=ceph-username=client.admin \
          --from-literal=ceph-secret="$ADMIN_KEY" \
          --dry-run=client -o yaml | kubectl apply -f - >/dev/null

        # Mon endpoints, in the two shapes Rook and ceph-csi each expect.
        CSI_CFG=$(jq -nc --arg mon "$MON_ADDR" \
          '[{clusterID:"${ns}",monitors:[$mon],cephFS:{subvolumeGroup:""},rbd:{}}]')
        kubectl create configmap rook-ceph-mon-endpoints -n ${ns} \
          --from-literal=data="${host}=$MON_ADDR" \
          --from-literal=maxMonId=0 \
          --from-literal=mapping='{}' \
          --from-literal=csi-cluster-config-json="$CSI_CFG" \
          --dry-run=client -o yaml | kubectl apply -f - >/dev/null

        echo "published Ceph credentials for fsid $FSID, mon $MON_ADDR"
      '';
    };

    # Re-runs so credentials appear once both Ceph and k3s are up, and so a mon
    # address change propagates without a reboot.
    systemd.timers.yolab-ceph-csi-secrets = {
      wantedBy = ["timers.target"];
      timerConfig = {
        OnBootSec = "4min";
        OnUnitActiveSec = "10min";
      };
    };
  };
}
