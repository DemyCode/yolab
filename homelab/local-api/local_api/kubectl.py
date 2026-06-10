import json
import shlex
import subprocess

from local_api.models.ceph import OsdUsage

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
    """Run a ceph CLI command inside the mgr pod with admin credentials."""
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
        f"printf '[global]\\nmon_host = v2:[{mon_ip}]:3300\\n' > /tmp/ceph.conf && "
        f"ceph -c /tmp/ceph.conf --keyring /tmp/k --name client.admin "
        + " ".join(shlex.quote(a) for a in args)
    )
    result = subprocess.run(
        ["kubectl", "exec", "-n", _CEPH_NS, ceph_mgr_pod(), "--", "bash", "-c", shell_cmd],
        capture_output=True, text=True, timeout=30,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip())
    return result.stdout


def _ceph_exporter_url() -> str:
    result = subprocess.run(
        ["kubectl", "get", "svc", "-n", _CEPH_NS, "rook-ceph-exporter",
         "-o", "jsonpath={.spec.clusterIP}"],
        capture_output=True, text=True, timeout=10,
    )
    ip = result.stdout.strip()
    return f"http://[{ip}]:9926/metrics" if ip else ""


def ceph_osd_df() -> dict[int, OsdUsage]:
    """Returns per-OSD usage as {osd_id: OsdUsage} via Prometheus exporter."""
    import urllib.request
    try:
        url = _ceph_exporter_url()
        if not url:
            return {}
        with urllib.request.urlopen(url, timeout=5) as resp:
            text = resp.read().decode()

        total: dict[int, int] = {}
        used: dict[int, int] = {}
        for line in text.splitlines():
            if line.startswith("ceph_osd_stat_bytes{"):
                osd_id = int(line.split('"osd.')[1].split('"')[0])
                total[osd_id] = int(float(line.split("} ")[1]))
            elif line.startswith("ceph_osd_stat_bytes_used{"):
                osd_id = int(line.split('"osd.')[1].split('"')[0])
                used[osd_id] = int(float(line.split("} ")[1]))

        result: dict[int, OsdUsage] = {}
        for osd_id in total:
            t = total[osd_id]
            u = used.get(osd_id, 0)
            result[osd_id] = OsdUsage(
                osd_id=osd_id,
                used_bytes=u,
                free_bytes=max(0, t - u),
                total_bytes=t,
                reweight=1.0,
            )
        return result
    except Exception:
        return {}


def ceph_osd_numpg() -> dict[int, int]:
    """Returns {osd_id: pg_count} via Prometheus exporter."""
    import urllib.request
    try:
        url = _ceph_exporter_url()
        if not url:
            return {}
        with urllib.request.urlopen(url, timeout=5) as resp:
            text = resp.read().decode()
        result: dict[int, int] = {}
        for line in text.splitlines():
            if line.startswith("ceph_osd_numpg{"):
                osd_id = int(line.split('"osd.')[1].split('"')[0])
                result[osd_id] = int(float(line.split("} ")[1]))
        return result
    except Exception:
        return {}
