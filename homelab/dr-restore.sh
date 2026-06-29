#!/usr/bin/env bash
# YoLab Disaster Recovery — restore a full cluster from B2 backup.
#
# Usage:
#   YOLAB_TOKEN=<your-token> YOLAB_PLATFORM_URL=https://... bash dr-restore.sh
#
# Or just run it and it will prompt for the values.
#
# What this script does:
#   1. Fetches B2 credentials from the YoLab platform using your account token
#   2. Downloads the latest etcd snapshot from B2
#   3. Installs K3s (if not already present)
#   4. Resets K3s to the snapshot (restores full cluster state)
#   5. Waits for the cluster to be ready
#   6. Prints the URL to open for the final PVC data restore

set -euo pipefail

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[+]${NC} $*"; }
warn()  { echo -e "${YELLOW}[!]${NC} $*"; }
error() { echo -e "${RED}[x]${NC} $*" >&2; exit 1; }

# ── Prerequisites ─────────────────────────────────────────────────────────────

for cmd in curl jq; do
  command -v "$cmd" >/dev/null 2>&1 || error "Required command not found: $cmd"
done

# ── Credentials ───────────────────────────────────────────────────────────────

PLATFORM_URL="${YOLAB_PLATFORM_URL:-}"
ACCOUNT_TOKEN="${YOLAB_TOKEN:-}"

if [ -z "$PLATFORM_URL" ]; then
  read -rp "YoLab platform URL (e.g. https://api.yolab.app): " PLATFORM_URL
fi
if [ -z "$ACCOUNT_TOKEN" ]; then
  read -rsp "YoLab account token: " ACCOUNT_TOKEN; echo
fi

PLATFORM_URL="${PLATFORM_URL%/}"

# ── Fetch B2 credentials from yolab-external ─────────────────────────────────

info "Fetching backup credentials from platform…"
S3_INFO=$(curl -sf -H "Authorization: Bearer ${ACCOUNT_TOKEN}" "${PLATFORM_URL}/storage/s3") \
  || error "Failed to fetch B2 credentials. Check your token and platform URL."

BUCKET=$(echo "$S3_INFO" | jq -r '.bucket_name')
ENDPOINT=$(echo "$S3_INFO" | jq -r '.endpoint')
ACCESS_KEY=$(echo "$S3_INFO" | jq -r '.access_key_id')
SECRET_KEY=$(echo "$S3_INFO" | jq -r '.secret_access_key')

[ "$BUCKET" = "null" ] || [ -z "$BUCKET" ] && error "B2 bucket not found. Has backup been enabled?"

S3_HOST="${ENDPOINT#https://}"
S3_HOST="${S3_HOST#http://}"

info "Bucket: ${BUCKET} @ ${ENDPOINT}"

# ── Find and download the latest etcd snapshot ───────────────────────────────

SNAPSHOT_DIR="/var/lib/rancher/k3s/server/db/snapshots"
mkdir -p "$SNAPSHOT_DIR"

info "Listing etcd snapshots in B2…"

# Use AWS-compatible list via curl with S3 REST API (unsigned listing won't work,
# so we use the B2 native API to list objects with the prefix "etcd-daily-").
B2_AUTH=$(curl -sf \
  -u "${ACCESS_KEY}:${SECRET_KEY}" \
  "https://api.backblazeb2.com/b2api/v2/b2_authorize_account") \
  || error "B2 auth failed — check B2 credentials."

B2_API_URL=$(echo "$B2_AUTH" | jq -r '.apiUrl')
B2_AUTH_TOKEN=$(echo "$B2_AUTH" | jq -r '.authorizationToken')
B2_BUCKET_ID=$(curl -sf \
  -H "Authorization: ${B2_AUTH_TOKEN}" \
  "${B2_API_URL}/b2api/v2/b2_list_buckets?accountId=$(echo "$B2_AUTH" | jq -r '.accountId')&bucketName=${BUCKET}" \
  | jq -r '.buckets[0].bucketId')

[ "$B2_BUCKET_ID" = "null" ] || [ -z "$B2_BUCKET_ID" ] && error "Bucket ${BUCKET} not found."

DOWNLOAD_URL=$(echo "$B2_AUTH" | jq -r '.downloadUrl')

# List objects with prefix etcd-daily-
FILES=$(curl -sf \
  -H "Authorization: ${B2_AUTH_TOKEN}" \
  "${B2_API_URL}/b2api/v2/b2_list_file_names?bucketId=${B2_BUCKET_ID}&prefix=etcd-daily-&maxFileCount=100" \
  | jq -r '.files[].fileName') \
  || error "Failed to list B2 objects."

[ -z "$FILES" ] && error "No etcd snapshots found in bucket (prefix: etcd-daily-)."

LATEST_SNAPSHOT=$(echo "$FILES" | sort | tail -1)
info "Latest snapshot: ${LATEST_SNAPSHOT}"

LOCAL_PATH="${SNAPSHOT_DIR}/${LATEST_SNAPSHOT}"

if [ -f "$LOCAL_PATH" ]; then
  warn "Snapshot already downloaded at ${LOCAL_PATH}, skipping download."
else
  info "Downloading snapshot…"
  curl -sf \
    -H "Authorization: ${B2_AUTH_TOKEN}" \
    "${DOWNLOAD_URL}/file/${BUCKET}/${LATEST_SNAPSHOT}" \
    -o "$LOCAL_PATH" \
    --progress-bar \
    || error "Download failed."
  info "Downloaded to ${LOCAL_PATH}"
fi

# ── Install K3s if not present ────────────────────────────────────────────────

if ! command -v k3s >/dev/null 2>&1; then
  info "Installing K3s…"
  curl -sfL https://get.k3s.io | INSTALL_K3S_EXEC="server --cluster-init" sh -
  sleep 5
else
  info "K3s already installed."
fi

# ── Restore etcd from snapshot ────────────────────────────────────────────────

warn "Stopping K3s and restoring cluster state from snapshot…"
warn "This will REPLACE the current cluster state."
read -rp "Proceed? [y/N] " confirm
[ "${confirm,,}" = "y" ] || error "Aborted."

systemctl stop k3s 2>/dev/null || true

info "Running cluster reset with snapshot restore…"
k3s server \
  --cluster-reset \
  --cluster-reset-restore-path="${LOCAL_PATH}" \
  2>&1 | tail -5

info "Starting K3s…"
systemctl start k3s

# ── Wait for cluster to be ready ─────────────────────────────────────────────

info "Waiting for cluster to be ready (this may take 30-60 seconds)…"
until kubectl get nodes >/dev/null 2>&1; do sleep 3; done
info "Cluster is up!"

until kubectl get nodes | grep -q " Ready"; do sleep 3; done
info "Node is Ready."

# ── Done ─────────────────────────────────────────────────────────────────────

NODE_IP=$(kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="ExternalIP")].address}' 2>/dev/null \
  || kubectl get nodes -o jsonpath='{.items[0].status.addresses[?(@.type=="InternalIP")].address}' 2>/dev/null \
  || echo "your-node-ip")

echo ""
echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo -e "${GREEN} Cluster state restored from backup!${NC}"
echo -e "${GREEN}━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━${NC}"
echo ""
echo "Your K8s objects (apps, configs, secrets) are back."
echo "Your PVC data (app files) still needs to be restored from B2."
echo ""
echo "Open your browser at:  http://${NODE_IP}:3000"
echo ""
echo "The Backup page will show 'Disaster Recovery Mode'."
echo "Click 'Restore All from Cloud' and wait for the data restore to complete."
echo ""
