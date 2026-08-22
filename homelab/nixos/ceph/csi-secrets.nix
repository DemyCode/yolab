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
      # NOTE: NixOS wraps `script` with `set -e` already, so the `set -uo
      # pipefail` below adds to it rather than replacing it — any unguarded
      # command that fails aborts the unit. Every "not ready yet" path here is
      # therefore an explicit `exit 0`, not a fallthrough.
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

        # Every mon in the map, not this node's own address.
        #
        # Hardcoding one mon made CSI depend on one specific machine: when that
        # machine reboots, every PVC mount on every *other* node fails, because
        # the only monitor the driver knows about is gone. ceph-csi is given a
        # list and tries them in turn, so this is the difference between "a node
        # rebooted" and "storage went away".
        #
        # The v1 endpoint (6789) because that is what librados expects from a
        # bare host:port; handing it the v2 port is a silent connection failure.
        MON_DUMP=$(ceph mon dump -f json 2>/dev/null || true)
        MON_JSON=$(printf '%s' "$MON_DUMP" \
          | jq -c '[.mons[].public_addrs.addrvec[] | select(.type == "v1") | .addr]' 2>/dev/null || true)
        if [ -z "$MON_JSON" ] || [ "$MON_JSON" = "[]" ]; then
          echo "no mon address in ceph mon dump — not publishing a broken CSI config"
          exit 0
        fi
        # Rook's own bookkeeping ConfigMap wants "name=addr,name=addr".
        MON_ENDPOINTS=$(printf '%s' "$MON_DUMP" | jq -r \
          '[.mons[] | .name as $n | (.public_addrs.addrvec[] | select(.type == "v1") | .addr) | "\($n)=\(.)"] | join(",")')

        # CSI needs restricted users, not client.admin. These caps are the ones
        # Rook's own import script grants; broader caps would hand every CSI pod
        # cluster-admin over Ceph.
        #
        # Create only what is missing, then read the key back. `ceph auth
        # get-or-create-key` does NOT return an existing key when the requested
        # caps differ — it fails with
        #   EINVAL: key for client.csi-cephfs-provisioner exists but cap mon does not match
        # which is exactly what happens here, because Rook's operator creates
        # these same users itself (it can: the rook-ceph-mon secret below hands
        # it admin credentials). Re-asserting our caps would start a tug-of-war
        # between this timer and Rook's reconcile loop, flipping the caps back
        # and forth every few minutes. Whoever created the user owns its caps.
        ensure_key() {
          local entity="$1"
          shift
          if ! ceph auth get-key "$entity" >/dev/null 2>&1; then
            ceph auth get-or-create "$entity" "$@" >/dev/null
          fi
          ceph auth get-key "$entity"
        }

        CEPHFS_PROV=$(ensure_key client.csi-cephfs-provisioner \
          mon 'allow r' mgr 'allow rw' osd 'allow rw tag cephfs metadata=*')
        CEPHFS_NODE=$(ensure_key client.csi-cephfs-node \
          mon 'allow r' mgr 'allow rw' osd 'allow rw tag cephfs *=*' mds 'allow rw')
        RBD_PROV=$(ensure_key client.csi-rbd-provisioner \
          mon 'profile rbd' mgr 'allow rw' osd 'profile rbd')
        RBD_NODE=$(ensure_key client.csi-rbd-node \
          mon 'profile rbd' osd 'profile rbd')

        # Rook creates these same Secrets with type `kubernetes.io/rook`, and a
        # Secret's type is IMMUTABLE. Creating them as the default `Opaque` meant
        # Rook could never update them, and its CephCluster reconcile failed on
        # every pass for an hour:
        #   Secret "rook-csi-cephfs-node" is invalid:
        #     type: Invalid value: "kubernetes.io/rook": field is immutable
        # So match Rook's type. Same lesson as the cephx caps above: where Rook
        # also manages an object, conform to its shape rather than compete.
        # A Secret's type cannot be changed in place, so an existing one of the
        # wrong type has to be replaced rather than patched. Without this the
        # unit fails permanently on any cluster that already has Opaque secrets
        # from an earlier version — which is exactly what happened: the rebuild
        # itself reported the unit as failed.
        replace_if_wrong_type() {
          local have
          have=$(kubectl get secret "$1" -n ${ns} -o jsonpath='{.type}' 2>/dev/null || true)
          if [ -n "$have" ] && [ "$have" != "kubernetes.io/rook" ]; then
            echo "secret $1 has type $have, recreating as kubernetes.io/rook"
            kubectl delete secret "$1" -n ${ns} >/dev/null 2>&1 || true
          fi
        }

        apply_secret() {
          # `create --dry-run | apply` so this is an upsert: the unit re-runs on
          # every boot and must converge, not fail on "already exists".
          replace_if_wrong_type "$1"
          kubectl create secret generic "$1" -n ${ns} \
            --type=kubernetes.io/rook \
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
        replace_if_wrong_type rook-ceph-mon
        kubectl create secret generic rook-ceph-mon -n ${ns} \
          --type=kubernetes.io/rook \
          --from-literal=cluster-name=${ns} \
          --from-literal=fsid="$FSID" \
          --from-literal=admin-secret=admin-secret \
          --from-literal=mon-secret=mon-secret \
          --from-literal=ceph-username=client.admin \
          --from-literal=ceph-secret="$ADMIN_KEY" \
          --dry-run=client -o yaml | kubectl apply -f - >/dev/null

        CSI_CFG=$(jq -nc --argjson mons "$MON_JSON" \
          '[{clusterID:"${ns}",monitors:$mons,cephFS:{subvolumeGroup:""},rbd:{}}]')

        # Mon endpoints for Rook's own bookkeeping.
        kubectl create configmap rook-ceph-mon-endpoints -n ${ns} \
          --from-literal=data="$MON_ENDPOINTS" \
          --from-literal=maxMonId=0 \
          --from-literal=mapping='{}' \
          --from-literal=csi-cluster-config-json="$CSI_CFG" \
          --dry-run=client -o yaml | kubectl apply -f - >/dev/null

        # The one the CSI drivers ACTUALLY read. Both plugin pods mount
        # `rook-ceph-csi-config` as ceph-csi-config; rook-ceph-mon-endpoints is
        # not mounted by them at all. Writing only the latter left this at "[]",
        # and every PVC failed with
        #   missing configuration for cluster ID "rook-ceph"
        # Patched rather than replaced: Rook owns this ConfigMap and puts other
        # keys in it, and it does not fight us over this one.
        kubectl patch configmap rook-ceph-csi-config -n ${ns} --type merge \
          -p "$(jq -nc --arg c "$CSI_CFG" '{data:{"csi-cluster-config-json":$c}}')" \
          >/dev/null 2>&1 \
          || kubectl create configmap rook-ceph-csi-config -n ${ns} \
               --from-literal=csi-cluster-config-json="$CSI_CFG" \
               --dry-run=client -o yaml | kubectl apply -f - >/dev/null

        echo "published Ceph credentials for fsid $FSID, mons $MON_ENDPOINTS"
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
