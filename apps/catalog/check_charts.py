#!/usr/bin/env python3
"""Structural checks over every rendered app chart.

`helm lint` and `helm template` only prove the templates produce *some* YAML.
They accept a chart whose Caddy upstream points at a host that does not resolve,
whose two containers both try to bind :80, or whose volumeMount names a volume
nobody declared. All three of those shipped in this catalog and were found by
these assertions, not by helm.

So the question here is not "does it render" but "does the rendered thing
describe a workload that can actually run":

  - the gateway wiring is intact (wg-register + wireguard + caddy, in one pod)
  - only the tunnel sidecar is privileged
  - no two containers in a pod claim the same port
  - every volumeMount resolves to a declared volume
  - every reverse_proxy upstream resolves to a local container or a real Service
  - every Service selector matches a pod the chart creates
  - images are digest-pinned and never re-pull on restart
  - no Secret key renders empty
  - ACCOUNT_TOKEN reaches only wg-register/cleanup, and only by reference
  - a container reading YOLAB_FQDN/YOLAB_URL mounts /yolab AND has an init
    container that writes it
  - every `sh -c` container command is valid shell

Renders against the yolab-common in this working tree, not the published one, so
a library change is checked against the charts before it is released.

Usage:
    check_charts.py [chart-dir ...]     # default: every chart in this directory
"""

import glob
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import image_arches
import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
LIBRARY = os.path.join(HERE, "yolab-common")

LINT_VALUES = {
    "config.password": "PlaceholderPw2026",
    "config.admin_password": "PlaceholderPw2026",
    "config.admin_email": "admin@example.com",
    "config.app_secret": "PlaceholderPw2026",
    "config.rpc_password": "PlaceholderPw2026",
    "config.explorer_password": "PlaceholderPw2026",
    "config.app_key": "PlaceholderAppKey2026Placeholder",
    "config.api_key": "PlaceholderApiKey2026",
    "config.auth_secret_key": "PlaceholderAuthSecretKey2026Placeholder",
    "config.gateway_token": "PlaceholderGatewayToken2026Placeholder",
    "config.server_name": "example",
    "config.server_pass": "PlaceholderPw2026",
    "config.firefly_url": "http://firefly-iii.yolab-firefly-iii.svc.cluster.local:8080",
    "config.tts_url": "http://kokoro.yolab-kokoro.svc.cluster.local:8880/v1",
    "config.subdomain": "example",
    "config.vpn_private_key": "PlaceholderVpnKey2026=",
    "config.vpn_addresses": "10.64.0.2/32",
    "config.auth_users[0].username": "admin",
    "config.auth_users[0].password": "PlaceholderPw2026",
    "config.desktop_password": "PlaceholderPw2026",
    "config.sunshine_password": "PlaceholderPw2026",
}

VARIANTS = {
    "open-webui": [
        {
            "machines[0].name": "gpu-box",
            "machines[0].accelerator": "nvidia",
            "machines[1].name": "Radeon.Laptop",
            "machines[1].accelerator": "amd",
            "machines[2].name": "old-pc",
            "machines[2].accelerator": "cpu",
        },
        {
            "machines[0].name": "gpu-box",
            "machines[0].accelerator": "nvidia",
            "config.ollama_url": "http://ollama.yolab-ai.svc.cluster.local:11434",
        },
    ],
    "jellyfin": [
        {"gpu.name": "nuc", "gpu.accelerator": "intel"},
        {"gpu.name": "gpu-box", "gpu.accelerator": "nvidia"},
    ],
    "immich": [
        {"gpu.name": "gpu-box", "gpu.accelerator": "nvidia"},
        {"gpu.name": "nuc", "gpu.accelerator": "intel"},
    ],
    "frigate": [
        {"gpu.name": "nuc", "gpu.accelerator": "intel"},
        {"gpu.name": "gpu-box", "gpu.accelerator": "nvidia"},
    ],
    "ollama": [
        {"machine.name": "gpu-box", "machine.accelerator": "nvidia"},
        {"machine.name": "old-radeon", "machine.accelerator": "vulkan"},
        {"machine.name": "vega", "machine.accelerator": "amd"},
    ],
    "steam-headless": [
        {"machine.name": "gpu-box", "machine.accelerator": "nvidia"},
        {"machine.name": "nuc", "machine.accelerator": "intel"},
        {"machine.name": "gt710", "machine.accelerator": "nvidia-legacy"},
    ],
}

GATEWAY_CONTAINERS = ("wireguard", "caddy")
WORKLOAD_KINDS = ("Deployment", "DaemonSet", "StatefulSet")
OWN_IMAGES = "ghcr.io/demycode/"
with open(os.path.join(HERE, "image_arches.json")) as _f:
    ARCHES = json.load(_f)
TOKEN_CONTAINERS = ("wg-register", "cleanup", "yolab-env")


class Failures:
    def __init__(self):
        self.items = []

    def __call__(self, app, msg):
        self.items.append(f"{app}: {msg}")

    def __len__(self):
        return len(self.items)


def chart_field(chart_yaml, field):
    for line in chart_yaml.splitlines():
        if line.startswith(f"{field}:"):
            return line.split(":", 1)[1].strip().strip('"')
    return None


def render(chart_dir, library_tgz, workdir, extra=None):
    """helm template the chart against the local library. Returns the YAML text.

    Renders from a copy: the chart has to gain a `charts/` directory holding the
    library, and the source tree is read-only when this runs from the nix store.
    """
    staged = os.path.join(workdir, os.path.basename(chart_dir.rstrip("/")))
    shutil.copytree(chart_dir, staged, dirs_exist_ok=True)
    os.makedirs(os.path.join(staged, "charts"), exist_ok=True)
    shutil.copy(library_tgz, os.path.join(staged, "charts"))

    cmd = ["helm", "template", "release", staged]
    for k, v in {**LINT_VALUES, **(extra or {})}.items():
        cmd += ["--set", f"{k}={v}"]
    out = subprocess.run(cmd, capture_output=True, text=True, check=False)
    shutil.rmtree(staged, ignore_errors=True)
    if out.returncode != 0:
        return None, out.stderr.strip()
    return out.stdout, None


def check_db_init_is_idempotent(app, script, container, fail):
    """Run a database-setup init script against a stub and prove it converges.

    `sh -n` proves a script parses. It says nothing about the trap this exists
    for: an init that CREATES a database and then configures it, branching on
    whether the file is already there. If the create succeeds and the configure
    does not, every later run sees the file, takes the other branch, and tries to
    modify something that was never set up. The container then crash-loops
    forever and the only way out is deleting the volume. filebrowser shipped
    exactly that and wedged an app through nine restarts.

    So the three states are driven for real, with a stub standing in for the
    binary: a fresh volume, a fully set-up one, and the half-finished one in
    between. All three must exit 0, because an init container is re-run on every
    restart and cannot assume which one it woke up in.
    """
    with tempfile.TemporaryDirectory() as tmp:
        stub_dir = os.path.join(tmp, "bin")
        os.makedirs(stub_dir)
        stub = os.path.join(stub_dir, "filebrowser")
        with open(stub, "w") as fh:
            fh.write(
                "#!/bin/sh\n"
                'case "$1 $2" in\n'
                '  "config init") [ "$STATE" = fresh ] && touch "$DBPATH"; exit 0 ;;\n'
                '  "users add")   [ "$STATE" = ready ] && exit 1; exit 0 ;;\n'
                '  "users update") [ "$STATE" = ready ] && exit 0; exit 1 ;;\n'
                "esac\n"
                "exit 0\n"
            )
        os.chmod(stub, 0o755)

        db_dir = os.path.join(tmp, "db")
        os.makedirs(db_dir)
        db_path = os.path.join(db_dir, "filebrowser.db")
        local = script.replace("/db/filebrowser.db", db_path)

        for state in ("fresh", "ready", "halfway"):
            if state == "fresh":
                if os.path.exists(db_path):
                    os.remove(db_path)
            else:
                open(db_path, "w").close()
            env = dict(
                os.environ,
                PATH=stub_dir + os.pathsep + os.environ["PATH"],
                STATE=state,
                DBPATH=db_path,
                FB_ADMIN_PASSWORD="x",
            )
            run = subprocess.run(
                ["sh", "-c", local],
                capture_output=True,
                text=True,
                env=env,
                check=False,
            )
            if run.returncode != 0:
                fail(
                    app,
                    f"container {container}: database init is not re-runnable — "
                    f"with an existing database in the '{state}' state it exits "
                    f"{run.returncode}, so the pod crash-loops and cannot recover "
                    f"without deleting the volume: {run.stdout.strip()} "
                    f"{run.stderr.strip()}",
                )


def check_arch(app, workload, fail, arches):
    spec = workload["spec"]["template"]["spec"]
    images = [
        c["image"]
        for c in (spec.get("containers") or []) + (spec.get("initContainers") or [])
        if not c["image"].startswith(OWN_IMAGES)
    ]
    unknown = [i for i in images if i not in arches]
    for image in unknown:
        fail(
            app,
            f"{image} is not in image_arches.json, so nothing says which processors "
            f"it runs on: run image_arches.py",
        )
    x86_only = [i for i in images if i in arches and "arm64" not in arches[i]]
    pinned = (spec.get("nodeSelector") or {}).get("kubernetes.io/arch") == "amd64"
    if x86_only and not pinned:
        fail(
            app,
            f"{workload['kind']} {workload['metadata']['name']} runs {x86_only[0]}, "
            f"which has no arm64 build, but is not pinned to kubernetes.io/arch: amd64",
        )


def check(app, docs, fail, chart_yaml="", schema=None, arches=None):
    kinds = {}
    for d in docs:
        kinds.setdefault(d["kind"], []).append(d)
    check_containers(app, docs, fail)

    has_caddy = any(
        "Caddyfile" in (c.get("data") or {}) for c in kinds.get("ConfigMap", [])
    )

    own_claims = [
        c for c in kinds.get("PersistentVolumeClaim", []) if not is_folder_claim(c)
    ]
    for kind, want in (("PersistentVolumeClaim", 1), ("Job", 1)):
        got = len(
            own_claims if kind == "PersistentVolumeClaim" else kinds.get(kind, [])
        )
        if got != want:
            fail(app, f"expected {want} {kind}, got {got}")

    job = (kinds.get("Job") or [{}])[0]
    if (
        job.get("metadata", {}).get("annotations", {}).get("helm.sh/hook")
        != "pre-delete"
    ):
        fail(app, "uninstall Job is not a pre-delete hook")

    deploys = {d["metadata"]["name"]: d for d in kinds.get("Deployment", [])}

    tunnel_pods = [
        (n, d["spec"]["template"]["spec"])
        for n, d in deploys.items()
        if any(
            c["name"] == "wg-register"
            for c in d["spec"]["template"]["spec"].get("initContainers") or []
        )
    ]
    off_pods = [
        (n, d["spec"]["template"]["spec"])
        for n, d in deploys.items()
        if any(
            c["name"] == "yolab-env"
            for c in d["spec"]["template"]["spec"].get("initContainers") or []
        )
        and any(
            c["name"] == "caddy"
            for c in d["spec"]["template"]["spec"].get("containers") or []
        )
    ]
    if not tunnel_pods and len(off_pods) == 1:
        gw_name, gw = off_pods[0]
        conts = {c["name"]: c for c in gw.get("containers") or []}
        if "wireguard" in conts:
            fail(app, "the YoLab address is off but the tunnel sidecar still runs")
    elif len(tunnel_pods) != 1:
        fail(
            app,
            f"expected exactly one pod running wg-register, found {len(tunnel_pods)}",
        )
        return
    else:
        gw_name, gw = tunnel_pods[0]
        conts = {c["name"]: c for c in gw.get("containers") or []}

        for req in GATEWAY_CONTAINERS if has_caddy else ("wireguard",):
            if req not in conts:
                fail(app, f"pod {gw_name} missing {req} container")

        if (
            conts.get("wireguard", {}).get("securityContext", {}).get("privileged")
            is not True
        ):
            fail(app, "wireguard sidecar is not privileged (the tunnel cannot come up)")

    for name, c in conts.items():
        if name != "wireguard" and c.get("securityContext", {}).get("privileged"):
            fail(app, f"container {name} is privileged but is not the tunnel sidecar")

    for dname, d in deploys.items():
        spec = d["spec"]["template"]["spec"]

        inits = {c["name"]: c for c in spec.get("initContainers") or []}
        writes_yolab_env = any(n in inits for n in ("wg-register", "yolab-env"))
        for c in spec.get("containers") or []:
            uses = "YOLAB_FQDN" in json.dumps(c) or "YOLAB_URL" in json.dumps(c)
            if not uses:
                continue
            if not any(m["mountPath"] == "/yolab" for m in c.get("volumeMounts") or []):
                fail(
                    app,
                    f"pod {dname}: container {c['name']} reads YOLAB_* but does "
                    f"not mount /yolab",
                )
            if not writes_yolab_env:
                fail(
                    app,
                    f"pod {dname}: container {c['name']} reads YOLAB_* but no init "
                    f"container writes /yolab/env (needs wg-register or yolab-env)",
                )

        for c in (spec.get("containers") or []) + (spec.get("initContainers") or []):
            cmd = c.get("command") or []
            if (
                len(cmd) >= 3
                and cmd[0] in ("/bin/sh", "sh", "/bin/bash")
                and cmd[1] == "-c"
            ):
                syntax = subprocess.run(
                    ["sh", "-n"],
                    input=cmd[2],
                    capture_output=True,
                    text=True,
                    check=False,
                )
                if syntax.returncode != 0:
                    fail(
                        app,
                        f"pod {dname}: container {c['name']} command is not valid "
                        f"shell: {syntax.stderr.strip()}",
                    )
                    continue
                if "users add" in cmd[2] or "users update" in cmd[2]:
                    check_db_init_is_idempotent(app, cmd[2], c["name"], fail)

        seen = {}
        for c in spec.get("containers") or []:
            for p in c.get("ports") or []:
                cp = p["containerPort"]
                if cp in seen:
                    fail(
                        app,
                        f"pod {dname}: containerPort {cp} claimed by both "
                        f"{seen[cp]} and {c['name']}",
                    )
                seen[cp] = c["name"]

        vols = {v["name"] for v in spec.get("volumes") or []}
        for c in (spec.get("containers") or []) + (spec.get("initContainers") or []):
            for m in c.get("volumeMounts") or []:
                if m["name"] not in vols:
                    fail(
                        app,
                        f"pod {dname}: container {c['name']} mounts undeclared "
                        f"volume {m['name']}",
                    )

    if has_caddy:
        caddyfile = next(
            c["data"]["Caddyfile"]
            for c in kinds["ConfigMap"]
            if "Caddyfile" in (c.get("data") or {})
        )
        ups = re.findall(r"reverse_proxy\s+(\S+)", caddyfile)
        if not ups:
            fail(app, "Caddyfile has no reverse_proxy directive")
        svcs = {s["metadata"]["name"]: s for s in kinds.get("Service", [])}
        for up in ups:
            if "{{" in up or "}}" in up:
                fail(
                    app,
                    f"upstream {up!r} still contains an unrendered template expression",
                )
                continue
            host, _, port = up.rpartition(":")
            if host == "localhost":
                if not [n for n in conts if n not in GATEWAY_CONTAINERS]:
                    fail(
                        app,
                        f"upstream {up} is localhost but no app container runs in {gw_name}",
                    )
                if port in ("80", "443"):
                    fail(
                        app, f"upstream port {port} collides with Caddy in the same pod"
                    )
            elif host not in svcs:
                fail(
                    app,
                    f"upstream {up} names Service '{host}' which the chart does not create",
                )
            else:
                exposed = {str(p["port"]) for p in svcs[host]["spec"]["ports"]}
                if port not in exposed:
                    fail(
                        app,
                        f"upstream {up}: Service {host} exposes {sorted(exposed)}, not {port}",
                    )

    workloads = [d for kind in WORKLOAD_KINDS for d in kinds.get(kind, [])]
    pod_labels = [
        tuple(sorted(d["spec"]["template"]["metadata"]["labels"].items()))
        for d in workloads
    ]
    for s in kinds.get("Service", []):
        sel = tuple(sorted(s["spec"]["selector"].items()))
        if not any(all(kv in lbl for kv in sel) for lbl in pod_labels):
            fail(
                app,
                f"Service {s['metadata']['name']} selector {dict(sel)} matches no pod",
            )

    if not image_arches.is_disabled(chart_yaml):
        for d in workloads + kinds.get("Job", []):
            check_arch(app, d, fail, ARCHES if arches is None else arches)

    for d in workloads + kinds.get("Job", []):
        spec = d["spec"]["template"]["spec"]
        for c in (spec.get("containers") or []) + (spec.get("initContainers") or []):
            if "@sha256:" not in c["image"]:
                fail(app, f"image not digest-pinned: {c['image']}")
            if c.get("imagePullPolicy") != "IfNotPresent":
                fail(app, f"{c['name']}: imagePullPolicy is {c.get('imagePullPolicy')}")

            for e in c.get("env") or []:
                if e.get("name") != "ACCOUNT_TOKEN":
                    continue
                if c["name"] not in TOKEN_CONTAINERS:
                    fail(app, f"ACCOUNT_TOKEN exposed to container {c['name']}")
                if "value" in e:
                    fail(app, f"ACCOUNT_TOKEN passed by value in {c['name']}")

    for s in kinds.get("Secret", []):
        for k, v in (s.get("stringData") or {}).items():
            if v == "":
                fail(app, f"Secret key {k} rendered empty")

    check_sourced_secrets_reach_the_program(app, docs, fail)

    for d in docs:
        spec = (d.get("spec", {}).get("template", {}) or {}).get("spec", {})
        for c in spec.get("containers") or []:
            if "livenessProbe" in c:
                fail(app, f"container {c['name']} has a livenessProbe — none by design")
            if "startupProbe" in c:
                fail(app, f"container {c['name']} has a startupProbe — none by design")

    renders_file_explorer = any(
        c.get("name") == "file-explorer-init"
        for d in docs
        for c in (
            d.get("spec", {}).get("template", {}).get("spec", {}).get("initContainers")
            or []
        )
    )
    if renders_file_explorer:
        outputs = ((schema or {}).get("properties") or {}).get("outputs") or {}
        declared = outputs.get("properties") or {}
        for key, when in EXPLORER_OUTPUTS.items():
            if key not in declared:
                fail(
                    app,
                    f"renders the file explorer but its schema's outputs "
                    f"do not declare {key}",
                )
            elif declared[key].get("when") != when:
                fail(
                    app,
                    f"output {key} is not conditioned on the file explorer being "
                    f"on, so an app installed without it waits for it forever",
                )


DATABASE_IMAGES = re.compile(
    r"^(postgres|postgis|pgvector|mariadb|mysql|redis|valkey|mongo|minio)$"
)
EXPLORER_HEADER_UP = re.compile(r"header_up\s+(\S+)\s+\{http\.auth\.user\.id\}")


def image_name(image):
    repo = image.split("@", 1)[0].rsplit("/", 1)[-1]
    return repo.split(":", 1)[0]


def pod_specs(docs):
    for d in docs:
        if d.get("kind") != "Deployment":
            continue
        yield d["metadata"]["name"], d["spec"]["template"]["spec"]


ENV_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_.-]*$")
PORT_NAME = re.compile(r"^(?=.{1,15}$)(?=.*[a-z])[a-z0-9]+(-[a-z0-9]+)*$")


def check_containers(app, docs, fail):
    for pod, spec in pod_specs(docs):
        containers = (spec.get("initContainers") or []) + (spec.get("containers") or [])
        names = [c["name"] for c in containers]
        for dup in sorted({n for n in names if names.count(n) > 1}):
            fail(app, f"pod {pod} has two containers named {dup}")
        for c in containers:
            for p in c.get("ports") or []:
                if "name" in p and not PORT_NAME.match(str(p["name"])):
                    fail(
                        app,
                        f"container {c['name']} in pod {pod} names a port {p['name']!r}; "
                        f"the API server refuses it (max 15 chars, a-z 0-9 and single '-')",
                    )
            for e in c.get("env") or []:
                if not ENV_NAME.match(str(e.get("name"))):
                    fail(
                        app,
                        f"container {c['name']} in pod {pod} sets env {e.get('name')!r}, "
                        f"which is not a variable name",
                    )


def claim_mounts(spec, container):
    claims = {
        v["name"]: v["persistentVolumeClaim"]["claimName"]
        for v in spec.get("volumes") or []
        if "persistentVolumeClaim" in v
    }
    for m in container.get("volumeMounts") or []:
        if m["name"] in claims:
            yield claims[m["name"]], m.get("subPath", ""), m.get("readOnly") is True


def state_claimed(spec, sidecar, state):
    for c in list(spec.get("initContainers") or []) + [sidecar]:
        script = "\n".join((c.get("command") or []) + (c.get("args") or []))
        namespace = {
            e["name"]: ((e.get("valueFrom") or {}).get("fieldRef") or {}).get(
                "fieldPath"
            )
            for e in c.get("env") or []
        }.get("POD_NAMESPACE")
        if namespace != "metadata.namespace":
            continue
        for m in c.get("volumeMounts") or []:
            if not m.get("subPath", "").strip('"').endswith(f"/{state}"):
                continue
            if f"claim_state {m['mountPath']}\n" in script + "\n":
                return True
    return False


def explorer_pod(docs):
    for name, spec in pod_specs(docs):
        if any(
            c["name"] == "file-explorer-init" for c in spec.get("initContainers") or []
        ):
            return name, spec
    return None, None


def explorer_config(docs):
    for d in docs:
        if d.get("kind") != "ConfigMap":
            continue
        if not d["metadata"]["name"].endswith("-file-explorer"):
            continue
        text = (d.get("data") or {}).get("config.yaml")
        if text is not None:
            return yaml.safe_load(text) or {}
    return None


def check_file_explorer(app, docs, fail):
    pod_name, spec = explorer_pod(docs)
    if spec is None:
        return
    explorer = next(
        (c for c in spec.get("containers") or [] if c["name"] == "file-explorer"),
        None,
    )
    if explorer is None:
        fail(
            app,
            f"pod {pod_name} runs file-explorer-init but no file-explorer container",
        )
        return

    config = explorer_config(docs)
    if config is None:
        fail(app, "the file-explorer container has no rendered config.yaml ConfigMap")
        return

    http = config.get("http") or {}
    if http.get("listen") != "127.0.0.1":
        fail(
            app,
            f"file-explorer listens on {http.get('listen')!r}, not 127.0.0.1 — any pod "
            f"could reach it without the Caddy login",
        )

    methods = (config.get("auth") or {}).get("methods") or {}
    proxy = methods.get("proxy") or {}
    if (methods.get("password") or {}).get("enabled") is not False:
        fail(app, "file-explorer keeps its own password login next to the Caddy one")
    if proxy.get("enabled") is not True or not proxy.get("header"):
        fail(app, "file-explorer does not take the signed-in user from Caddy")

    caddyfile = next(
        (
            c["data"]["Caddyfile"]
            for c in docs
            if c.get("kind") == "ConfigMap" and "Caddyfile" in (c.get("data") or {})
        ),
        "",
    )
    handed = EXPLORER_HEADER_UP.findall(caddyfile)
    if proxy.get("header") and proxy["header"] not in handed:
        fail(
            app,
            f"Caddy never sets {proxy['header']} from its basic_auth user, so a "
            f"browser could send that header itself",
        )

    read_only = [(claim, sub) for claim, sub, ro in claim_mounts(spec, explorer) if ro]
    for dname, dspec in pod_specs(docs):
        for c in dspec.get("containers") or []:
            if not DATABASE_IMAGES.match(image_name(c["image"])):
                continue
            for claim, sub, _ in claim_mounts(dspec, c):
                guarded = any(
                    claim == rclaim
                    and (rsub == "" or sub == rsub or sub.startswith(rsub + "/"))
                    for rclaim, rsub in read_only
                )
                mounted = any(
                    claim == eclaim for eclaim, _, _ in claim_mounts(spec, explorer)
                )
                if mounted and not guarded:
                    fail(
                        app,
                        f"database {dname}/{c['name']} keeps its files in {sub!r}, "
                        f"which file-explorer can write — add it to "
                        f"yolab.fileExplorer.protect",
                    )


def check_file_explorer_read_only(app, docs, fail):
    _, spec = explorer_pod(docs)
    if spec is None:
        return
    explorer = next(
        (c for c in spec.get("containers") or [] if c["name"] == "file-explorer"),
        None,
    )
    if explorer is None:
        return
    for claim, sub, ro in claim_mounts(spec, explorer):
        if not ro:
            fail(
                app,
                f"read-only file-explorer still mounts {claim}:{sub!r} writable",
            )
    for source in ((explorer_config(docs) or {}).get("server") or {}).get(
        "sources"
    ) or []:
        cfg = source.get("config") or {}
        if cfg.get("readOnly") is not True:
            fail(
                app,
                f"read-only file-explorer leaves source {source.get('name')} writable",
            )
        granted = [
            k
            for k in ("modify", "create", "delete")
            if (cfg.get("defaultPermissions") or {}).get(k)
        ]
        if granted:
            fail(
                app,
                f"read-only file-explorer still grants {', '.join(granted)} on "
                f"source {source.get('name')}",
            )


SOURCED = re.compile(r"^\s*\.\s+(\S+)", re.MULTILINE)
BARE_SECRET = re.compile(r"printf '([A-Z_][A-Z0-9_]*)=%s")


PRIVATE_ACCESS = {
    "tor_enabled": {
        "container": "tor",
        "state": "tor",
        "output": "tor_url",
        "port": 18792,
    },
    "tailscale_enabled": {
        "container": "tailscale",
        "state": "tailscale",
        "output": "tailscale_url",
        "port": 18791,
    },
}
PRIVATE_ACCESS_ON = {
    "tor_enabled": {"config.tor_enabled": "true"},
    "tailscale_enabled": {
        "config.tailscale_enabled": "true",
        "config.tailscale_auth_key": "tskey-auth-placeholder",
    },
}


def offers_file_explorer(schema):
    config = ((schema.get("properties") or {}).get("config") or {}).get(
        "properties"
    ) or {}
    return "file_explorer_enabled" in config


def offered_private_access(schema):
    config = ((schema.get("properties") or {}).get("config") or {}).get(
        "properties"
    ) or {}
    return [k for k in PRIVATE_ACCESS if k in config]


def check_private_access_offer(app, schema, values_text, fail):
    offered = offered_private_access(schema)
    if not offered:
        return
    props = schema.get("properties") or {}
    config = (props.get("config") or {}).get("properties") or {}
    if "auth_enabled" in config:
        fail(
            app,
            "offers Tor or Tailscale next to an Authelia login: those addresses "
            "reach the app directly and would skip the login",
        )
    values = yaml.safe_load(values_text or "") or {}
    gateway = ((values.get("yolab") or {}).get("gateway")) or {}
    if gateway.get("caddyfile"):
        fail(
            app,
            "offers Tor or Tailscale with its own Caddyfile: their entry points "
            "only know the single upstream a standard gateway proxies to",
        )
    elif not gateway.get("upstream"):
        fail(app, "offers Tor or Tailscale but declares no yolab.gateway.upstream")
    outputs = (props.get("outputs") or {}).get("properties") or {}
    for key in offered:
        out = outputs.get(PRIVATE_ACCESS[key]["output"])
        when = (((out or {}).get("when") or {}).get("properties") or {}).get(key)
        if out is None:
            fail(
                app,
                f"offers {key} but has no {PRIVATE_ACCESS[key]['output']} output, "
                f"so its address is never shown",
            )
        elif when != {"const": True}:
            fail(
                app,
                f"outputs.{PRIVATE_ACCESS[key]['output']} must only show when "
                f"{key} is on",
            )


def configmap_data(docs, suffix, key):
    for d in docs:
        if d.get("kind") == "ConfigMap" and d["metadata"]["name"].endswith(suffix):
            return (d.get("data") or {}).get(key)
    return None


def check_private_access(app, docs, offered, fail):
    pod = next(
        (
            (name, spec)
            for name, spec in pod_specs(docs)
            if any(c["name"] == "caddy" for c in spec.get("containers") or [])
        ),
        None,
    )
    if pod is None:
        fail(app, "offers Tor or Tailscale but renders no gateway pod with Caddy")
        return
    pod_name, spec = pod
    containers = {c["name"]: c for c in spec.get("containers") or []}
    caddyfile = configmap_data(docs, "-caddy", "Caddyfile") or ""
    for key in offered:
        want = PRIVATE_ACCESS[key]
        c = containers.get(want["container"])
        if c is None:
            fail(
                app,
                f"{key} is on but pod {pod_name} has no {want['container']} "
                f"container next to Caddy",
            )
            continue
        if (c.get("securityContext") or {}).get("privileged"):
            fail(app, f"the {want['container']} container must not run privileged")
        state = [s for _, s, _ in claim_mounts(spec, c)]
        if not any(s.strip('"').endswith(f"/{want['state']}") for s in state):
            fail(
                app,
                f"the {want['container']} container keeps no state on the app's "
                f"volume, so its address changes on every restart and is lost on restore",
            )
        if not state_claimed(spec, c, want["state"]):
            fail(
                app,
                f"nothing claims the {want['container']} state for this namespace "
                f"before it starts, so a copy would run with the original's identity",
            )
        site = f"http://:{want['port']} {{\n  bind 127.0.0.1\n"
        if site not in caddyfile:
            fail(
                app,
                f"Caddy has no loopback-only entry point on port {want['port']} "
                f"for {want['container']}",
            )
    if "tor_enabled" in offered:
        torrc = configmap_data(docs, "-tor", "torrc") or ""
        if (
            f"HiddenServicePort 80 127.0.0.1:{PRIVATE_ACCESS['tor_enabled']['port']}"
            not in torrc
        ):
            fail(app, "the onion service does not point at Caddy's Tor entry point")
        if "SocksPort 0" not in torrc:
            fail(app, "the Tor sidecar must not open a SOCKS proxy inside the pod")
    if "tailscale_enabled" in offered:
        serve = configmap_data(docs, "-tailscale", "serve.json") or "{}"
        try:
            web = json.loads(serve).get("Web") or {}
        except json.JSONDecodeError:
            web = {}
        targets = [
            h.get("Proxy")
            for site in web.values()
            for h in (site.get("Handlers") or {}).values()
        ]
        wanted = f"http://127.0.0.1:{PRIVATE_ACCESS['tailscale_enabled']['port']}"
        if targets != [wanted]:
            fail(app, f"Tailscale serves {targets}, not Caddy's entry point {wanted}")
        env = {
            e["name"]: e.get("value")
            for e in containers.get("tailscale", {}).get("env") or []
        }
        if env.get("TS_USERSPACE") != "true":
            fail(app, "Tailscale must run in userspace: the pod has no TUN device")
        if env.get("TS_KUBE_SECRET") != "":
            fail(
                app,
                "Tailscale would keep its identity in a Kubernetes Secret the app "
                "may not write; TS_KUBE_SECRET must be empty",
            )


API_KEY_GUARD = (
    '@yolab_without_api_key not header Authorization "Bearer {$YOLAB_API_KEY}"'
)


def offers_api_key(schema):
    config = ((schema.get("properties") or {}).get("config") or {}).get(
        "properties"
    ) or {}
    return "api_key_enabled" in config


def check_api_key_offer(app, schema, fail):
    if not offers_api_key(schema):
        return
    props = schema.get("properties") or {}
    config = (props.get("config") or {}).get("properties") or {}
    if "auth_enabled" in config:
        fail(
            app,
            "offers an API key next to an Authelia login: the login's site has "
            "no API key guard",
        )
    out = ((props.get("outputs") or {}).get("properties") or {}).get("api_key")
    if out is None:
        fail(app, "offers an API key but has no api_key output, so nobody can read it")
        return
    if out.get("format") != "secret":
        fail(app, "outputs.api_key must be shown as a secret")
    when = ((out.get("when") or {}).get("properties") or {}).get("api_key_enabled")
    if when != {"const": True}:
        fail(app, "outputs.api_key must only show when api_key_enabled is on")


def check_api_key(app, docs, upstream, fail):
    caddyfile = configmap_data(docs, "-caddy", "Caddyfile") or ""
    sites = len(re.findall(rf"reverse_proxy {re.escape(upstream)}(\s|$)", caddyfile))
    guarded = caddyfile.count(API_KEY_GUARD)
    inits = [
        c["name"]
        for _, spec in pod_specs(docs)
        for c in spec.get("initContainers") or []
    ]
    if "api-key-init" not in inits:
        fail(app, "the API key is on but no api-key-init container creates it")
    if sites == 0 or guarded < sites:
        fail(
            app,
            f"the API key is on but only {guarded} of the {sites} sites Caddy "
            f"proxies check it",
        )


EXPLORER_WAYS = {
    "file_explorer_tor_enabled": {
        "container": "file-explorer-tor",
        "state": "file-explorer-tor",
        "port": 18794,
    },
    "file_explorer_tailscale_enabled": {
        "container": "file-explorer-tailscale",
        "state": "file-explorer-tailscale",
        "port": 18793,
    },
}
EXPLORER_ENABLED = {"config.file_explorer_enabled": "true"}
EXPLORER_WAYS_ON = {
    **EXPLORER_ENABLED,
    "config.file_explorer_tor_enabled": "true",
    "config.file_explorer_tailscale_enabled": "true",
    "config.file_explorer_tailscale_auth_key": "tskey-auth-placeholder",
}


def check_file_explorer_ways(app, docs, fail):
    pod_name, spec = explorer_pod(docs)
    if spec is None:
        fail(app, "the file explorer's Tor and Tailscale are on but it does not run")
        return
    containers = {c["name"]: c for c in spec.get("containers") or []}
    caddyfile = configmap_data(docs, "-caddy", "Caddyfile") or ""
    for key, want in EXPLORER_WAYS.items():
        c = containers.get(want["container"])
        if c is None:
            fail(app, f"{key} is on but pod {pod_name} has no {want['container']}")
            continue
        if (c.get("securityContext") or {}).get("privileged"):
            fail(app, f"the {want['container']} container must not run privileged")
        state = [sub.strip('"') for _, sub, _ in claim_mounts(spec, c)]
        if not any(sub.endswith(f"/{want['state']}") for sub in state):
            fail(
                app,
                f"the {want['container']} container keeps no state of its own on "
                f"the app's volume, so its address changes on every restart",
            )
        if not state_claimed(spec, c, want["state"]):
            fail(
                app,
                f"nothing claims the {want['container']} state for this namespace "
                f"before it starts, so a copy would run with the original's identity",
            )
        site = f"http://:{want['port']} {{\n  bind 127.0.0.1\n  basic_auth {{"
        if site not in caddyfile:
            fail(
                app,
                f"Caddy has no loopback-only entry point behind the explorer's "
                f"login on port {want['port']} for {want['container']}",
            )
    torrc = configmap_data(docs, "-file-explorer", "torrc") or ""
    port = EXPLORER_WAYS["file_explorer_tor_enabled"]["port"]
    if f"HiddenServicePort 80 127.0.0.1:{port}" not in torrc:
        fail(
            app, "the explorer's onion service does not point at its Caddy entry point"
        )
    if "SocksPort 0" not in torrc:
        fail(app, "the explorer's Tor must not open a SOCKS proxy inside the pod")
    serve = configmap_data(docs, "-file-explorer", "serve.json") or "{}"
    try:
        web = json.loads(serve).get("Web") or {}
    except json.JSONDecodeError:
        web = {}
    targets = [
        h.get("Proxy")
        for site in web.values()
        for h in (site.get("Handlers") or {}).values()
    ]
    tailscale_port = EXPLORER_WAYS["file_explorer_tailscale_enabled"]["port"]
    wanted = f"http://127.0.0.1:{tailscale_port}"
    if targets != [wanted]:
        fail(app, f"the explorer's Tailscale serves {targets}, not {wanted}")
    env = {
        e["name"]: e.get("value")
        for e in containers.get("file-explorer-tailscale", {}).get("env") or []
    }
    if env.get("TS_USERSPACE") != "true":
        fail(app, "the explorer's Tailscale must run in userspace")
    if env.get("TS_KUBE_SECRET") != "":
        fail(
            app,
            "the explorer's Tailscale would keep its identity in a Kubernetes Secret",
        )


def check_yolab_off(app, docs, fail):
    for name, spec in pod_specs(docs):
        names = {c["name"] for c in (spec.get("containers") or [])} | {
            c["name"] for c in (spec.get("initContainers") or [])
        }
        if names & {"wg-register", "wireguard", "file-explorer"}:
            fail(
                app,
                f"pod {name} still registers a tunnel, runs WireGuard or the file "
                f"explorer with the YoLab address off",
            )
    caddyfile = configmap_data(docs, "-caddy", "Caddyfile") or ""
    if "YOLAB_FQDN" in caddyfile:
        fail(app, "Caddy still serves the YoLab address with it switched off")


def check_sourced_secrets_reach_the_program(app, docs, fail):
    bare = sorted(set(BARE_SECRET.findall(json.dumps(docs))))
    if not bare:
        return
    for d in docs:
        spec = (d.get("spec", {}).get("template", {}) or {}).get("spec", {})
        for c in spec.get("containers") or []:
            script = "\n".join((c.get("command") or [])[2:] + (c.get("args") or []))
            sourced = [p for p in SOURCED.findall(script) if p != "/yolab/env"]
            if not sourced or "exec " not in script or "set -a" in script:
                continue
            for name in bare:
                if f"${name}" in script or f"${{{name}}}" in script:
                    continue
                fail(
                    app,
                    f"container {c['name']} sources {', '.join(sourced)} and execs a "
                    f"program that never sees {name} — `set -a` before sourcing",
                )


EXPLORER_ON = {"properties": {"file_explorer_enabled": {"const": True}}}
EXPLORER_PUBLIC = {
    "properties": {"file_explorer_enabled": {"const": True}},
    "anyOf": [
        {
            "properties": {"file_explorer_yolab_enabled": {"const": True}},
            "required": ["file_explorer_yolab_enabled"],
        },
        {
            "not": {"required": ["file_explorer_yolab_enabled"]},
            "properties": {"yolab_enabled": {"not": {"const": False}}},
        },
    ],
}


def explorer_way_on(switch):
    return {
        "properties": {
            "file_explorer_enabled": {"const": True},
            switch: {"const": True},
        }
    }


EXPLORER_OUTPUTS = {
    "file_explorer_url": EXPLORER_PUBLIC,
    "file_explorer_password": EXPLORER_ON,
    "file_explorer_tor_url": explorer_way_on("file_explorer_tor_enabled"),
    "file_explorer_tailscale_url": explorer_way_on("file_explorer_tailscale_enabled"),
}
OUTPUT_FORMATS = {"text", "uri", "secret", "multiline"}
LEGACY_ANNOTATIONS = ("yolab.io/uischema", "yolab.io/outputs")


YOLAB_ON = {"properties": {"yolab_enabled": {"const": True}}}


def check_yolab_switch(app, config_obj, props, fail):
    config = config_obj.get("properties") or {}
    if "yolab_enabled" not in config:
        return
    if any(p.get("format") == "tunnel" for p in config.values()):
        fail(
            app,
            "the subdomain is shown even with the YoLab address off; it belongs "
            "in the yolab_enabled switch's on branch",
        )
    branches = ((config_obj.get("dependencies") or {}).get("yolab_enabled") or {}).get(
        "oneOf"
    ) or []
    on = next(
        (
            b
            for b in branches
            if (b.get("properties") or {}).get("yolab_enabled") == {"const": True}
        ),
        None,
    )
    off = next(
        (
            b
            for b in branches
            if (b.get("properties") or {}).get("yolab_enabled") == {"const": False}
        ),
        None,
    )
    if on is None or off is None:
        fail(app, "yolab_enabled needs an on branch and an off branch")
        return
    on_props = on.get("properties") or {}
    tunnel = [n for n, p in on_props.items() if p.get("format") == "tunnel"]
    if len(tunnel) != 1 or tunnel[0] not in (on.get("required") or []):
        fail(app, "the YoLab address on branch must require exactly one subdomain")
    token = on_props.get("yolab_token") or {}
    if token.get("format") != "yolab-token" or token.get("writeOnly") is not True:
        fail(
            app,
            "the YoLab address on branch needs a write-only yolab_token the box fills in",
        )
    if "yolab_enabled" not in (off.get("required") or []):
        fail(
            app,
            "the off branch must require yolab_enabled, or an install from before "
            "the switch matches both branches and its upgrade is refused",
        )
    outputs = (props.get("outputs") or {}).get("properties") or {}
    for key, out in outputs.items():
        logs = (out.get("source") or {}).get("logs") or ""
        if logs.startswith("YOLAB_OUTPUT url ") and out.get("when") != YOLAB_ON:
            fail(
                app,
                f"outputs.{key} is the YoLab address and must only show when "
                f"yolab_enabled is on",
            )


COLLECTIONS = {
    "start-here",
    "replace-google",
    "family",
    "watch-and-listen",
    "privacy",
    "for-developers",
    "play",
}
GITHUB_REPO = re.compile(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$")
TAGLINE_MAX = 60


def check_store_annotations(app, annotations, fail):
    tagline = annotations.get("yolab.io/tagline")
    if not isinstance(tagline, str) or not tagline.strip():
        fail(app, "Chart.yaml has no yolab.io/tagline, the one line the store shows")
    elif len(tagline) > TAGLINE_MAX:
        fail(app, f"yolab.io/tagline is {len(tagline)} characters, over {TAGLINE_MAX}")
    github = annotations.get("yolab.io/github")
    if github is not None and not GITHUB_REPO.match(str(github)):
        fail(app, f"yolab.io/github {github!r} is not an owner/repo path")
    if "yolab.io/disabled" in annotations:
        reason = annotations.get("yolab.io/disabled")
        if not isinstance(reason, str) or not reason.strip():
            fail(app, "yolab.io/disabled is set but does not say why the app is hidden")
    for slug in str(annotations.get("yolab.io/collections") or "").split(","):
        if slug.strip() and slug.strip() not in COLLECTIONS:
            fail(
                app,
                f"yolab.io/collections names {slug.strip()!r}, not a known collection",
            )


def check_schema(app, schema, chart_yaml, fail):
    try:
        annotations = (yaml.safe_load(chart_yaml) or {}).get("annotations") or {}
    except yaml.YAMLError:
        annotations = {}
    check_store_annotations(app, annotations, fail)
    for legacy in LEGACY_ANNOTATIONS:
        if legacy in annotations:
            fail(
                app,
                f"Chart.yaml still carries {legacy} — the form and the outputs "
                f"are declared in values.schema.json now",
            )

    props = schema.get("properties") or {}
    if "yolab" in props:
        fail(
            app,
            "values.schema.json describes the platform's `yolab` values — "
            "they are injected by YoLab, not chosen by whoever installs the app",
        )

    config_obj = props.get("config") or {}
    config = config_obj.get("properties") or {}
    branches = {}
    yolab_dep = (config_obj.get("dependencies") or {}).get("yolab_enabled") or {}
    for branch in yolab_dep.get("oneOf") or []:
        for name, prop in (branch.get("properties") or {}).items():
            if name != "yolab_enabled":
                branches[name] = prop
    tunnels = sorted(
        {
            n
            for n, p in {**config, **branches}.items()
            if isinstance(p, dict) and p.get("format") == "tunnel"
        }
    )
    if len(tunnels) > 1:
        fail(app, f"more than one address field: {', '.join(tunnels)}")
    check_yolab_switch(app, config_obj, props, fail)
    for name, prop in config.items():
        if prop.get("generate") and not prop.get("writeOnly"):
            fail(app, f"config.{name} is generated but not marked writeOnly")

    outputs = (props.get("outputs") or {}).get("properties") or {}
    for key, out in outputs.items():
        where = f"outputs.{key}"
        if not isinstance(out.get("title"), str) or not out["title"].strip():
            fail(app, f"{where} has no title to show")
        if out.get("format", "text") not in OUTPUT_FORMATS:
            fail(
                app,
                f"{where} has format {out.get('format')!r}, not one of "
                f"{sorted(OUTPUT_FORMATS)}",
            )
        source = out.get("source")
        if not isinstance(source, dict) or len(source) != 1:
            fail(
                app,
                f'{where} needs exactly one source: {{"logs": ...}} or {{"config": ...}}',
            )
        elif "logs" in source:
            try:
                if re.compile(source["logs"]).groups < 1:
                    fail(app, f"{where}: its logs pattern has no capture group")
            except (re.error, TypeError) as e:
                fail(app, f"{where}: its logs pattern does not compile ({e})")
        elif "config" in source:
            field = source["config"]
            if field not in config:
                fail(app, f"{where} shows config.{field}, which does not exist")
            elif not config[field].get("generate"):
                fail(
                    app,
                    f"{where} shows config.{field}, which the person typed "
                    f"themselves — only generated values need showing",
                )
        else:
            fail(app, f"{where} has an unknown source {sorted(source)}")
        if "when" in out and not isinstance(out["when"], (dict, bool)):
            fail(app, f"{where}: `when` must be a schema")


def provided_kinds(schema):
    return set((schema.get("x-yolab-provides") or {}).keys())


def wanted_kinds(schema):
    found = set()

    def walk(node):
        if isinstance(node, dict):
            if node.get("format") == "service-url":
                found.add(node.get("x-yolab-service") or "")
            for v in node.values():
                walk(v)
        elif isinstance(node, list):
            for v in node:
                walk(v)

    walk((schema.get("properties") or {}).get("config") or {})
    return found


def check_provides(app, schema, docs, fail):
    services = {
        d["metadata"]["name"]: {
            p.get("port") for p in (d.get("spec") or {}).get("ports") or []
        }
        for d in docs
        if d.get("kind") == "Service"
    }
    for kind, spec in (schema.get("x-yolab-provides") or {}).items():
        name, port = spec.get("service"), spec.get("port")
        if not name or not isinstance(port, int):
            fail(
                app, f"x-yolab-provides {kind} needs a service name and a numeric port"
            )
        elif port not in services.get(name, set()):
            fail(
                app,
                f"x-yolab-provides {kind} points at Service {name} port {port}, "
                f"which the chart does not render",
            )


def check_links(wanted, provided, fail):
    for app, kinds in sorted(wanted.items()):
        for kind in sorted(kinds):
            if not kind:
                fail(app, "a service-url field does not name its x-yolab-service")
            elif not any(kind in p for p in provided.values()):
                fail(app, f"a service-url field wants {kind}, which no chart provides")


FOLDER_LABEL = "yolab.io/folder"
FOLDER_PATTERN = "^([a-z0-9]([a-z0-9-]*[a-z0-9])?)?$"
FOLDER_PROBE = "probe-folder"


def is_folder_claim(doc):
    return FOLDER_LABEL in ((doc.get("metadata") or {}).get("labels") or {})


def folder_fields(schema):
    found = {}

    def walk(node):
        if isinstance(node, dict):
            for name, prop in (node.get("properties") or {}).items():
                if isinstance(prop, dict) and prop.get("format") == "folder":
                    found[name] = prop
            for v in node.values():
                walk(v)
        elif isinstance(node, list):
            for v in node:
                walk(v)

    walk((schema.get("properties") or {}).get("config") or {})
    return found


def check_folder_fields(app, schema, fail):
    for name, prop in sorted(folder_fields(schema).items()):
        where = f"config.{name}"
        if prop.get("type") != "string":
            fail(app, f"{where} is a folder field but not a string")
        if prop.get("default", "") != "":
            fail(
                app,
                f"{where} must default to empty: an app works on its own and a "
                f"folder is something the person chooses",
            )
        if prop.get("pattern") != FOLDER_PATTERN:
            fail(app, f"{where} needs the folder name pattern {FOLDER_PATTERN}")
        if not str(prop.get("title") or "").strip():
            fail(app, f"{where} has no title to show")


def folder_mounts(docs, claim):
    for pod, spec in pod_specs(docs):
        volumes = {
            v["name"]
            for v in spec.get("volumes") or []
            if (v.get("persistentVolumeClaim") or {}).get("claimName") == claim
        }
        for c in (spec.get("initContainers") or []) + (spec.get("containers") or []):
            for m in c.get("volumeMounts") or []:
                if m["name"] in volumes:
                    yield pod, c["name"], m


def check_folders(app, field, docs, fail):
    claim_name = f"folder-{FOLDER_PROBE}"
    claims = [
        d
        for d in docs
        if d.get("kind") == "PersistentVolumeClaim" and is_folder_claim(d)
    ]
    if [c["metadata"]["name"] for c in claims] != [claim_name]:
        fail(
            app,
            f"with config.{field} set the chart must render exactly one folder claim "
            f"named {claim_name}, got {[c['metadata']['name'] for c in claims]}",
        )
        return
    claim = claims[0]
    meta, spec = claim["metadata"], claim.get("spec") or {}
    namespace = meta.get("namespace") or "default"
    if meta["labels"][FOLDER_LABEL] != FOLDER_PROBE:
        fail(app, f"the folder claim's {FOLDER_LABEL} label is not the folder name")
    if spec.get("storageClassName") != "":
        fail(
            app,
            "the folder claim must have storageClassName \"\" — it binds to the "
            "folder YoLab mounts, never to a new empty volume",
        )
    if spec.get("volumeName") != f"{namespace}.folder-{FOLDER_PROBE}":
        fail(
            app,
            f"the folder claim must bind volumeName {{{{ .Release.Namespace }}}}.folder-<folder>, "
            f"got {spec.get('volumeName')!r}",
        )
    if spec.get("accessModes") != ["ReadWriteMany"]:
        fail(app, "the folder claim must be ReadWriteMany: other apps mount it too")
    mounts = list(folder_mounts(docs, claim_name))
    if not mounts:
        fail(app, f"config.{field} is chosen but no container mounts the folder")
    for pod, container, m in mounts:
        if m.get("mountPath") != f"/data/{FOLDER_PROBE}":
            fail(
                app,
                f"{pod}/{container} mounts the folder at {m.get('mountPath')!r}, not "
                f"/data/<folder> — every app must see the same paths",
            )
        if m.get("subPath"):
            fail(
                app,
                f"{pod}/{container} mounts only part of the folder; hardlinks need it whole",
            )


SETUP_KEYS = {"title", "tagline", "main", "folders", "apps"}
SETUP_APP_KEYS = {"chart", "settings", "folders"}
PLAIN_NAME = re.compile(r"^[a-z0-9]([a-z0-9-]{0,38}[a-z0-9])?$")


def check_setup(setup_id, setup, schemas, fail):
    where = f"setup {setup_id}"
    if not isinstance(setup, dict):
        fail(where, "is not a mapping")
        return
    if not PLAIN_NAME.match(setup_id):
        fail(where, "its file name is not a plain name")
    for key in sorted(set(setup) - SETUP_KEYS):
        fail(where, f"unknown key {key}")
    if not str(setup.get("title") or "").strip():
        fail(where, "has no title")
    folders = setup.get("folders") or {}
    apps = setup.get("apps") or {}
    if not apps:
        fail(where, "has no apps")
    if setup.get("main") is not None and setup["main"] not in apps:
        fail(where, f"main names {setup['main']}, which is not one of its apps")
    for key, folder in folders.items():
        if not PLAIN_NAME.match(str(key)):
            fail(where, f"folder {key!r} is not a plain name")
        if not str((folder or {}).get("title") or "").strip():
            fail(where, f"folder {key} has no title")
    for key, app in apps.items():
        app = app or {}
        if not PLAIN_NAME.match(str(key)):
            fail(where, f"app {key!r} is not a plain name")
        for extra in sorted(set(app) - SETUP_APP_KEYS):
            fail(where, f"app {key}: unknown key {extra}")
        chart = app.get("chart")
        if chart not in schemas:
            fail(
                where,
                f"app {key} installs {chart!r}, which is not a chart in this catalog",
            )
            continue
        offered = folder_fields(schemas[chart])
        for field, folder in (app.get("folders") or {}).items():
            if field not in offered:
                fail(where, f"app {key}: {chart} has no folder field {field}")
            if folder not in folders:
                fail(
                    where,
                    f"app {key}: {field} uses folder {folder}, "
                    f"which the setup does not list",
                )
        config = (schemas[chart].get("properties") or {}).get("config") or {}
        props = config.get("properties") or {}
        for setting in app.get("settings") or {}:
            if setting not in props:
                fail(where, f"app {key}: {chart} has no setting {setting}")


def check_setups(setups_dir, schemas, fail):
    for path in sorted(Path(setups_dir).glob("*.yaml")):
        try:
            setup = yaml.safe_load(path.read_text())
        except yaml.YAMLError as e:
            fail(f"setup {path.stem}", f"is not valid YAML: {e}")
            continue
        check_setup(path.stem, setup, schemas, fail)


def main(argv):
    chart_dirs = argv[1:] or sorted(
        d
        for d in glob.glob(os.path.join(HERE, "*/"))
        if os.path.isfile(os.path.join(d, "Chart.yaml"))
        and "type: library" not in Path(d, "Chart.yaml").read_text()
    )
    if not chart_dirs:
        print("no charts found", file=sys.stderr)
        return 1

    lib_version = chart_field(Path(LIBRARY, "Chart.yaml").read_text(), "version")
    fail = Failures()

    with tempfile.TemporaryDirectory() as tmp:
        subprocess.run(
            [
                "helm",
                "package",
                LIBRARY,
                "--version",
                lib_version,
                "--destination",
                tmp,
            ],
            check=True,
            capture_output=True,
        )
        library_tgz = glob.glob(os.path.join(tmp, "yolab-common-*.tgz"))[0]

        wanted, provided = {}, {}
        schemas = {}
        for chart_dir in chart_dirs:
            app = os.path.basename(chart_dir.rstrip("/"))
            text = Path(chart_dir, "Chart.yaml").read_text()

            declared = re.search(
                r"- name: yolab-common\s*\n\s*version:\s*\"?([^\"\n]+)", text
            )
            if declared and declared.group(1).strip() != lib_version:
                fail(
                    app,
                    f"depends on yolab-common {declared.group(1).strip()}, "
                    f"but this tree has {lib_version}",
                )

            rendered, err = render(chart_dir, library_tgz, tmp)
            if rendered is None:
                fail(
                    app,
                    f"helm template failed: {err.splitlines()[-1] if err else 'unknown'}",
                )
                continue
            try:
                docs = [d for d in yaml.safe_load_all(rendered) if d]
            except yaml.YAMLError as e:
                fail(app, f"rendered invalid YAML: {e}")
                continue
            schema_path = Path(chart_dir, "values.schema.json")
            try:
                schema = json.loads(schema_path.read_text())
            except FileNotFoundError:
                schema = {}
            except json.JSONDecodeError as e:
                fail(app, f"values.schema.json is not valid JSON: {e}")
                schema = {}
            check_schema(app, schema, text, fail)
            schemas[app] = schema
            wanted[app] = wanted_kinds(schema)
            provided[app] = provided_kinds(schema)
            check(app, docs, fail, text, schema)
            check_provides(app, schema, docs, fail)
            check_folder_fields(app, schema, fail)
            if any(
                is_folder_claim(d)
                for d in docs
                if d.get("kind") == "PersistentVolumeClaim"
            ):
                fail(app, "renders a folder claim although no folder was chosen")
            for field in sorted(folder_fields(schema)):
                chosen, err = render(
                    chart_dir, library_tgz, tmp, {f"config.{field}": FOLDER_PROBE}
                )
                if chosen is None:
                    fail(
                        app,
                        f"helm template with config.{field} set failed: "
                        f"{err.splitlines()[-1] if err else 'unknown'}",
                    )
                    continue
                chosen_docs = [d for d in yaml.safe_load_all(chosen) if d]
                check(app, chosen_docs, fail, text, schema)
                check_folders(app, field, chosen_docs, fail)
            check_file_explorer(app, docs, fail)
            values_path = Path(chart_dir, "values.yaml")
            check_private_access_offer(
                app,
                schema,
                values_path.read_text() if values_path.exists() else "",
                fail,
            )
            check_api_key_offer(app, schema, fail)
            values = (
                yaml.safe_load(values_path.read_text()) if values_path.exists() else {}
            )
            upstream = (((values or {}).get("yolab") or {}).get("gateway") or {}).get(
                "upstream"
            ) or ""
            if offers_api_key(schema):
                check_api_key(app, docs, upstream, fail)
            offered = offered_private_access(schema)
            if offered:
                extra = {}
                for key in offered:
                    extra.update(PRIVATE_ACCESS_ON[key])
                variant, err = render(chart_dir, library_tgz, tmp, extra)
                if variant is None:
                    fail(
                        app,
                        f"helm template with Tor/Tailscale on failed: "
                        f"{err.splitlines()[-1] if err else 'unknown'}",
                    )
                else:
                    variant_docs = [d for d in yaml.safe_load_all(variant) if d]
                    check(app, variant_docs, fail, text, schema)
                    check_private_access(app, variant_docs, offered, fail)
                    if offers_api_key(schema):
                        check_api_key(app, variant_docs, upstream, fail)
                config_props = (
                    (schema.get("properties") or {}).get("config") or {}
                ).get("properties") or {}
                if "yolab_enabled" in config_props:
                    off, err = render(
                        chart_dir,
                        library_tgz,
                        tmp,
                        {**extra, "config.yolab_enabled": "false"},
                    )
                    if off is None:
                        fail(
                            app,
                            f"helm template with the YoLab address off failed: "
                            f"{err.splitlines()[-1] if err else 'unknown'}",
                        )
                    else:
                        off_docs = [d for d in yaml.safe_load_all(off) if d]
                        check(app, off_docs, fail, text, schema)
                        check_private_access(app, off_docs, offered, fail)
                        check_yolab_off(app, off_docs, fail)

            for extra in VARIANTS.get(app, []):
                variant, err = render(chart_dir, library_tgz, tmp, extra)
                if variant is None:
                    fail(
                        app,
                        f"helm template with {extra} failed: "
                        f"{err.splitlines()[-1] if err else 'unknown'}",
                    )
                    continue
                check(
                    app,
                    [d for d in yaml.safe_load_all(variant) if d],
                    fail,
                    text,
                    schema,
                )

            if not offers_file_explorer(schema):
                continue
            enabled, err = render(chart_dir, library_tgz, tmp, EXPLORER_ENABLED)
            if enabled is None:
                fail(
                    app,
                    f"helm template with the file explorer on failed: "
                    f"{err.splitlines()[-1] if err else 'unknown'}",
                )
                continue
            enabled_docs = [d for d in yaml.safe_load_all(enabled) if d]
            if explorer_pod(enabled_docs)[1] is None:
                fail(app, "the file explorer is switched on but does not run")
                continue
            check(app, enabled_docs, fail, text, schema)
            check_file_explorer(app, enabled_docs, fail)
            ways, err = render(chart_dir, library_tgz, tmp, EXPLORER_WAYS_ON)
            if ways is None:
                fail(
                    app,
                    f"helm template with the file explorer's Tor and Tailscale on "
                    f"failed: {err.splitlines()[-1] if err else 'unknown'}",
                )
            else:
                ways_docs = [d for d in yaml.safe_load_all(ways) if d]
                check(app, ways_docs, fail, text, schema)
                check_file_explorer(app, ways_docs, fail)
                check_file_explorer_ways(app, ways_docs, fail)
            rendered, err = render(
                chart_dir,
                library_tgz,
                tmp,
                {**EXPLORER_ENABLED, "config.file_explorer_read_only": "true"},
            )
            if rendered is None:
                fail(
                    app,
                    f"helm template with a read-only file-explorer failed: "
                    f"{err.splitlines()[-1] if err else 'unknown'}",
                )
                continue
            check_file_explorer_read_only(
                app, [d for d in yaml.safe_load_all(rendered) if d], fail
            )

    if not argv[1:]:
        check_links(wanted, provided, fail)
        check_setups(os.path.join(HERE, "setups"), schemas, fail)
    print(f"checked {len(chart_dirs)} charts")
    for f in fail.items:
        print("FAIL " + f)
    print(f"FAILURES: {len(fail)}" if len(fail) else "all assertions passed")
    return 1 if len(fail) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
