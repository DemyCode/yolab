import os

PLATFORM = os.environ.get("YOLAB_PLATFORM", "nixos")
REPO_PATH = os.environ.get("YOLAB_REPO_PATH", "/etc/nixos")
FLAKE_TARGET = os.environ.get("YOLAB_FLAKE_TARGET", "yolab")
DOMAIN = os.environ.get("YOLAB_DOMAIN", "homelab.local")
CLUSTER_CONFIG_PATH = os.environ.get("YOLAB_CONFIG", "/etc/yolab/config.toml")

NODE_ID = os.environ.get("NODE_ID", "")
WG_IPV6 = os.environ.get("WG_IPV6", "")
WG_INTERFACE = os.environ.get("WG_INTERFACE", "wg0")
K3S_ROLE = os.environ.get("K3S_ROLE", "server")

KUBECONFIG = os.environ.get("KUBECONFIG", "/etc/rancher/k3s/k3s.yaml")
KUBECTL_ENV = {**os.environ, "KUBECONFIG": KUBECONFIG}

YOLAB_DATA_ROOT = "/yolab/data"
DISK_JSON_NAME = "yolab/disk.json"
CSI_SOCKET = "/run/yolab-csi/csi.sock"

# NFS exports
EXPORTS_FILE = "/etc/exports.d/yolab.exports"
NFS_MOUNT_ROOT = "/mnt/yolab-nfs"

# API routes
YOLAB_API_URL = os.environ.get("YOLAB_API_URL", "")
