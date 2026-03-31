import json
import os
import subprocess

_KUBECTL_ENV = {
    **os.environ,
    "KUBECONFIG": os.environ.get("KUBECONFIG", "/etc/rancher/k3s/k3s.yaml"),
}


def _run(*args: str) -> str:
    result = subprocess.run(list(args), capture_output=True, text=True, check=True, env=_KUBECTL_ENV)
    return result.stdout.strip()


def list_nodes() -> list[dict]:
    try:
        out = _run("kubectl", "get", "nodes", "-o", "json")
        data = json.loads(out)
        nodes = []
        for item in data.get("items", []):
            meta = item.get("metadata", {})
            status = item.get("status", {})
            addresses = {a["type"]: a["address"] for a in status.get("addresses", [])}
            conditions = {c["type"]: c["status"] for c in status.get("conditions", [])}
            nodes.append({
                "name": meta.get("name"),
                "ipv6": addresses.get("InternalIP"),
                "ready": conditions.get("Ready") == "True",
                "labels": meta.get("labels", {}),
            })
        return nodes
    except (subprocess.CalledProcessError, json.JSONDecodeError):
        return []


def scale_deployment(name: str, replicas: int, namespace: str = "default") -> None:
    subprocess.run(
        ["kubectl", "scale", "deployment", name,
         f"--replicas={replicas}", f"--namespace={namespace}"],
        check=False,
        env=_KUBECTL_ENV,
    )


def k3s_status() -> dict:
    try:
        out = _run("kubectl", "get", "nodes", "-o", "json")
        data = json.loads(out)
        items = data.get("items", [])
        ready = sum(
            1 for n in items
            if any(c["type"] == "Ready" and c["status"] == "True"
                   for c in n.get("status", {}).get("conditions", []))
        )
        return {"active": True, "total_nodes": len(items), "ready_nodes": ready}
    except Exception:
        return {"active": False, "total_nodes": 0, "ready_nodes": 0}
