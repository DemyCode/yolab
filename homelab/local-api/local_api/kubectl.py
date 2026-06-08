import json
import subprocess


def get_nodes() -> list[dict]:
    out = subprocess.check_output(["kubectl", "get", "nodes", "-o", "json"], text=True)
    return json.loads(out)["items"]


def get_node_ips() -> list[str]:
    ips = []
    for item in get_nodes():
        for addr in item["status"]["addresses"]:
            if addr["type"] == "InternalIP":
                ips.append(addr["address"])
                break
    return ips


def ceph_mgr_pod() -> str:
    result = subprocess.run(
        ["kubectl", "get", "pod", "-n", "rook-ceph", "-l", "app=rook-ceph-mgr",
         "-o", "jsonpath={.items[0].metadata.name}"],
        capture_output=True, text=True, timeout=10,
    )
    name = result.stdout.strip()
    if result.returncode != 0 or not name:
        raise RuntimeError("No rook-ceph-mgr pod found")
    return name
