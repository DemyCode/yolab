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
