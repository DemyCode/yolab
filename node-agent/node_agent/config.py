import os

YOLAB_API_URL = os.environ.get("YOLAB_API_URL", "")
NODE_ID = os.environ.get("NODE_ID", "")
WG_IPV6 = os.environ.get("WG_IPV6", "")
WG_INTERFACE = os.environ.get("WG_INTERFACE", "wg0")
K3S_ROLE = os.environ.get("K3S_ROLE", "server")
YOLAB_PLATFORM = os.environ.get("YOLAB_PLATFORM", "nixos")

YOLAB_DATA_ROOT = "/yolab/data"
DISK_JSON_NAME = "yolab/disk.json"
CSI_SOCKET = "/run/yolab-csi/csi.sock"
