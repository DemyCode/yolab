#!/bin/bash
# Nuclear Rook reset — wipes all state and bootstraps from scratch.
# Run as root on the homelab node.
set -euo pipefail

GREEN='\033[0;32m'; YELLOW='\033[1;33m'; RED='\033[0;31m'; NC='\033[0m'
log()  { echo -e "${GREEN}[fix-rook]${NC} $*"; }
warn() { echo -e "${YELLOW}[fix-rook]${NC} $*"; }

log "Step 1: scale down operator"
kubectl scale deploy -n rook-ceph rook-ceph-operator --replicas=0 2>/dev/null || true

log "Step 2: remove finalizers"
kubectl patch cephcluster    -n rook-ceph rook-ceph  -p '{"metadata":{"finalizers":[]}}' --type=merge 2>/dev/null || true
kubectl patch cephfilesystem -n rook-ceph yolab-fs   -p '{"metadata":{"finalizers":[]}}' --type=merge 2>/dev/null || true

log "Step 3: delete all Ceph workloads"
kubectl delete deploy,statefulset,daemonset -n rook-ceph -l rook_cluster=rook-ceph --grace-period=0 --force 2>/dev/null || true
kubectl delete jobs -n rook-ceph --all 2>/dev/null || true
kubectl delete pods -n rook-ceph --all --grace-period=0 --force 2>/dev/null || true

log "Step 4: delete all Ceph secrets (operator regenerates them)"
# Keep only K8s service-account tokens; delete every Rook-generated secret
kubectl get secrets -n rook-ceph -o name \
  | grep -v 'kubernetes.io/service-account-token\|sh.helm.release' \
  | xargs -r kubectl delete -n rook-ceph 2>/dev/null || true

log "Step 5: wipe host data"
systemctl stop yolab-system-osd 2>/dev/null || true
rm -rf /var/lib/rook/*
systemctl start yolab-system-osd
log "  loop device: $(losetup -j /var/lib/rook/system-osd.img)"

log "Step 6: scale operator back up"
kubectl scale deploy -n rook-ceph rook-ceph-operator --replicas=1

log "Step 7: wait for OSD deployment to appear (~60s)"
for i in $(seq 1 60); do
  if kubectl get deploy -n rook-ceph rook-ceph-osd-0 &>/dev/null; then
    log "  OSD deployment appeared"
    break
  fi
  echo -n "."
  sleep 2
done
echo

log "Step 8: patch CEPH_OSD_FLAVOR=classic now (CronJob covers future restarts)"
kubectl set env -n rook-ceph deploy/rook-ceph-osd-0 CEPH_OSD_FLAVOR=classic 2>/dev/null || warn "OSD deploy not ready yet — CronJob will patch it within 2 min"

log "Step 9: wait for OSD to be Running (~90s)"
for i in $(seq 1 45); do
  ready=$(kubectl get pods -n rook-ceph -l app=rook-ceph-osd --no-headers 2>/dev/null \
          | awk '{print $2}' | grep -c '^1/1$' || true)
  if [ "$ready" -ge 1 ]; then
    log "  OSD is Running!"
    break
  fi
  echo -n "."
  sleep 2
done
echo

log "Step 10: wait for CephFilesystem to be Ready (~60s)"
for i in $(seq 1 30); do
  phase=$(kubectl get cephfilesystem -n rook-ceph yolab-fs -o jsonpath='{.status.phase}' 2>/dev/null || true)
  if [ "$phase" = "Ready" ]; then
    log "  CephFilesystem is Ready!"
    break
  fi
  echo -n "."
  sleep 2
done
echo

log "Step 11: test PVC provisioning"
kubectl delete pvc rook-test -n default 2>/dev/null || true
kubectl apply -f - <<'YAML'
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: rook-test
spec:
  accessModes: [ReadWriteMany]
  storageClassName: yolab-cephfs
  resources:
    requests:
      storage: 1Gi
YAML

log "  waiting up to 3 min for PVC to bind..."
kubectl wait --for=jsonpath='{.status.phase}'=Bound pvc/rook-test --timeout=180s 2>/dev/null \
  && log "  PVC BOUND — Rook is working!" \
  || warn "  PVC still Pending — check: kubectl describe pvc rook-test"

kubectl delete pvc rook-test -n default 2>/dev/null || true

log "Done. Run 'kubectl get pods -n rook-ceph' to see cluster state."
