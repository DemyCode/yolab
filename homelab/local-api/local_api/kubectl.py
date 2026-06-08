import json
import shlex
import subprocess

_CEPH_NS = "rook-ceph"


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
        ["kubectl", "get", "pod", "-n", _CEPH_NS, "-l", "app=rook-ceph-mgr",
         "-o", "jsonpath={.items[0].metadata.name}"],
        capture_output=True, text=True, timeout=10,
    )
    name = result.stdout.strip()
    if result.returncode != 0 or not name:
        raise RuntimeError("No rook-ceph-mgr pod found")
    return name


def ceph_exec(*args: str) -> str:
    """Run a ceph CLI command inside the mgr pod with admin credentials.

    The mgr container has no ceph.conf and only its own keyring. We read
    the admin keyring + mon address from K8s and inject a minimal ceph.conf
    with ms_client_mode=secure to match the server's msgr2 requirement.
    """
    key_result = subprocess.run(
        ["kubectl", "get", "secret", "-n", _CEPH_NS, "rook-ceph-admin-keyring",
         "-o", "jsonpath={.data.keyring}"],
        capture_output=True, text=True, timeout=10,
    )
    if key_result.returncode != 0:
        raise RuntimeError(f"Cannot read admin keyring: {key_result.stderr.strip()}")
    keyring_b64 = key_result.stdout.strip()

    mon_result = subprocess.run(
        ["kubectl", "get", "svc", "-n", _CEPH_NS, "-l", "app=rook-ceph-mon",
         "-o", "jsonpath={.items[0].spec.clusterIP}"],
        capture_output=True, text=True, timeout=10,
    )
    if mon_result.returncode != 0 or not mon_result.stdout.strip():
        raise RuntimeError("Cannot find rook-ceph-mon service")
    mon_ip = mon_result.stdout.strip()

    shell_cmd = (
        f"echo {keyring_b64} | base64 -d > /tmp/k && "
        f"printf '[global]\\nms_client_mode = secure\\nms_cluster_mode = secure\\nms_service_mode = secure\\n' > /tmp/ceph.conf && "
        f"ceph -c /tmp/ceph.conf --keyring /tmp/k -m {mon_ip}:3300 "
        + " ".join(shlex.quote(a) for a in args)
    )
    result = subprocess.run(
        ["kubectl", "exec", "-n", _CEPH_NS, ceph_mgr_pod(), "--", "bash", "-c", shell_cmd],
        capture_output=True, text=True, timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    return result.stdout
