#!/usr/bin/env python3
"""Convert getumbrel/umbrel-apps manifests into YoLab app charts.

An Umbrel app is a directory holding `umbrel-app.yml` (metadata) and
`docker-compose.yml` (the workload). A YoLab app is a Helm chart built on the
`yolab-common` library: one PVC, one gateway Deployment carrying the tunnel,
Caddy, the file explorer and the app's own container, and a Deployment+Service
per backing service (postgres, redis, ...).

This script does that translation mechanically so the long tail of the Umbrel
catalogue does not have to be re-typed one chart at a time. It is deliberately
conservative: anything it cannot translate faithfully is skipped with a reason,
never emitted as a chart that would half-work.

    python3 import_umbrel.py --src /path/to/umbrel-apps
    python3 import_umbrel.py --only whoogle-search --force

What it will not touch (skip reasons, see `skip_reason`):
  - apps already in this catalogue (by name or alias)
  - apps whose Compose file reaches into another app's data directory
    (the Bitcoin/Lightning/Nostr ecosystem) — those need cross-app wiring
  - `network_mode`, `privileged`, `cap_add`, `devices`, `sysctls` — the gateway
    pod is deliberately unprivileged and shares a network namespace
  - apps that seed real config files out of the repo's `data/` dir
  - apps whose images are not digest-pinned
"""

import argparse
import json
import os
import re
import shlex
import shutil
import sys
import textwrap

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
DEFAULT_SRC = "/tmp/opencode/umbrel-apps"
LIB_VERSION = "0.1.15"

# Which processors each pinned image is published for, recorded by
# image_arches.py. Read when present so a workload running an x86-only image is
# pinned to amd64 the way check_charts.py requires.
try:
    with open(os.path.join(HERE, "image_arches.json")) as _f:
        ARCHES = json.load(_f)
except FileNotFoundError:
    ARCHES = {}


def needs_amd64(images):
    """True when an image cannot be assumed to run on an arm64 machine."""
    for image in images:
        if not image or image.startswith("ghcr.io/demycode/"):
            continue
        if "arm64" not in ARCHES.get(image, []):
            return True
    return False


# Umbrel app id -> the name this catalogue already knows it by. Only needed
# where the two do not match exactly.
ALIAS = {
    "file-browser": "filebrowser",
    "paperless": "paperless-ngx",
    "ittools": "it-tools",
    "changedetection-io": "changedetection",
}

# Placeholders Umbrel injects that have no YoLab equivalent but must resolve to
# something a program can parse. `localhost` is the honest answer: these are
# used to build self-referential URLs, and the request arrives through Caddy on
# the pod's own loopback.
LITERAL_ENV = {
    "DEVICE_DOMAIN_NAME": "localhost",
    "DEVICE_HOSTNAME": "localhost",
    "APP_DOMAIN": "localhost",
    "APP_HIDDEN_SERVICE": "localhost",
}

SECRET_HINTS = ("SEED", "PASSWORD", "SECRET", "KEY", "TOKEN", "SALT")

# image name -> the port it listens on when the Compose file does not say.
DEFAULT_PORTS = {
    "postgres": 5432,
    "postgis": 5432,
    "pgvector": 5432,
    "mariadb": 3306,
    "mysql": 3306,
    "redis": 6379,
    "valkey": 6379,
    "mongo": 27017,
    "minio": 9000,
    "rabbitmq": 5672,
    "memcached": 11211,
    "elasticsearch": 9200,
}

CATEGORY_ICON = {
    "files": "\U0001f4c1",
    "media": "\U0001f3ac",
    "bitcoin": "\u20bf",
    "networking": "\U0001f310",
    "automation": "\U0001f916",
    "developer": "\U0001f6e0",
    "development": "\U0001f6e0",
    "finance": "\U0001f4b0",
    "social": "\U0001f4ac",
    "productivity": "\u2705",
    "security": "\U0001f512",
    "gaming": "\U0001f3ae",
    "games": "\U0001f3ae",
    "ai": "\U0001f9e0",
    "photos": "\U0001f4f8",
    "music": "\U0001f3b5",
    "documents": "\U0001f4c4",
    "communication": "\U0001f4e7",
    "home": "\U0001f3e0",
    "utility": "\U0001f9f0",
    "utilities": "\U0001f9f0",
}

COLLECTIONS = {
    "media": "watch-and-listen",
    "photos": "replace-google",
    "files": "replace-google",
    "security": "privacy",
    "developer": "for-developers",
    "development": "for-developers",
    "gaming": "play",
    "games": "play",
}

CROSS_MARKERS = (
    "APP_BITCOIN",
    "APP_LIGHTNING",
    "APP_CORE_LIGHTNING",
    "APP_ELECTRS",
    "APP_MEMPOOL",
    "APP_LND",
    "APP_NETWORK",
    "APP_MONERO",
    "APP_LIBRE_RELAY",
    "APP_CORE_LIGHTNING",
    "TOR_PROXY",
    "CORE_LIGHTNING_PATH",
    "BITCOIND_PID",
    "NETWORK_IP",
)

PLACEHOLDER = re.compile(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}|\$([A-Za-z_][A-Za-z0-9_]*)")
DIGESTED = re.compile(r"@sha256:[0-9a-f]{64}\b")


def log(msg):
    print(msg, file=sys.stderr)


def load_yaml(path):
    with open(path) as f:
        return yaml.safe_load(f) or {}


def all_text(obj):
    return yaml.safe_dump(obj)


def use_underscore(name):
    return name.replace("-", "_")


# ---------------------------------------------------------------------------
# Skip decisions
# ---------------------------------------------------------------------------


SKIP = {
    "tdex": "its proxy and daemon need a Liquid node and Tor wiring the importer cannot produce",
}


def bump_patch(version):
    major, minor, patch = str(version).split(".")
    return f"{major}.{minor}.{int(patch) + 1}"


def carry_over(previous, chart_yaml):
    """A regenerated chart keeps its curated annotations and moves up a version."""
    return {
        **chart_yaml,
        "version": bump_patch(previous.get("version") or "0.1.0"),
        "annotations": {
            **(chart_yaml.get("annotations") or {}),
            **(previous.get("annotations") or {}),
        },
    }


def skip_reason(app_id, compose, existing):
    if app_id in SKIP:
        return SKIP[app_id]
    if app_id in existing or ALIAS.get(app_id) in existing:
        return "already in the catalogue"

    svcs = compose.get("services") or {}
    if not svcs:
        return "compose defines no services"

    text = all_text(compose)
    cross = sorted({m.group(1) or m.group(2) for m in PLACEHOLDER.finditer(text)})
    hits = [p for p in cross if p.startswith(CROSS_MARKERS)]
    if hits:
        return f"reaches into another app ({', '.join(hits[:3])})"

    for name, svc in svcs.items():
        svc = svc or {}
        mode = svc.get("network_mode")
        if mode and mode != "host":
            return f"service {name} uses network_mode {mode}"
        if svc.get("pid") == "host":
            return f"service {name} uses the host PID namespace"
        if svc.get("sysctls"):
            return f"service {name} sets kernel sysctls"
        so = svc.get("security_opt") or []
        if any("seccomp" in str(x) for x in so):
            return f"service {name} overrides the seccomp profile"
        if svc.get("build"):
            return f"service {name} is built from a Dockerfile"

    for name, svc in svcs.items():
        image = str((svc or {}).get("image") or "")
        if image and not DIGESTED.search(image):
            return f"image {image!r} is not digest-pinned"

    return None


# ---------------------------------------------------------------------------
# Compose -> workload
# ---------------------------------------------------------------------------


def env_items(env):
    if env is None:
        return []
    if isinstance(env, dict):
        return list(env.items())
    items = []
    for e in env:
        if isinstance(e, tuple):
            items.append(e)
            continue
        k, _, v = str(e).partition("=")
        items.append((k, v))
    return items


def rewrite_value(value, app_id, secrets):
    """Map an Umbrel env value onto Kubernetes env-expansion syntax.

    `${APP_SEED}`, `${APP_PASSWORD}` and the many app-specific secret names
    become `$(APP_SEED)`/`$(APP_PASSWORD)`, which the runtime expands from the
    env entries this same function arranges. Cross-service hostnames
    (`immich_postgres_1`) become the Service name this chart creates
    (`postgres`). Everything else becomes `$(NAME)` so that variables set
    earlier in the same container win.
    """
    if value is None:
        return ""
    if not isinstance(value, str):
        value = str(value)

    def repl(m):
        var = m.group(1) or m.group(2)
        if var in LITERAL_ENV:
            return LITERAL_ENV[var]
        if var in ("APP_SEED", "APP_PASSWORD"):
            secrets.add(var)
            return f"$({var})"
        if any(h in var for h in SECRET_HINTS):
            secrets.add("APP_SEED")
            return "$(APP_SEED)"
        if var.endswith("_PORT") and var.startswith("APP_"):
            return var
        if var in ("APP_DATA_DIR", "UMBREL_ROOT"):
            return "/data"
        return f"$({var})"

    return own_hostnames(PLACEHOLDER.sub(repl, value), app_id)


def own_hostnames(value, app_id):
    for prefix in {app_id, use_underscore(app_id)}:
        value = re.sub(
            r"(?<![A-Za-z0-9_-])"
            + re.escape(prefix)
            + r"_([a-z0-9][a-z0-9_-]*)_1(?![A-Za-z0-9_-])",
            r"\1",
            value,
        )
    return value


def k8s_env(env, app_id, secrets):
    items = env_items(env)
    out = []
    seen_secret = set()
    for k, v in items:
        k = str(k)
        if k in ("APP_SEED", "APP_PASSWORD"):
            continue
        out.append({"name": k, "value": rewrite_value(v, app_id, secrets)})
    # Secret-backed entries first are not required, but keep them explicit and
    # stable so `$(...)` expansion can see them (Kubernetes expands only from
    # variables declared earlier in the same list).
    for name, key in (("APP_SEED", "app_secret"), ("APP_PASSWORD", "admin_password")):
        if name in secrets and name not in seen_secret:
            out.insert(
                0,
                {
                    "name": name,
                    "valueFrom": {
                        "secretKeyRef": {
                            "name": "{{ .Release.Name }}-secrets",
                            "key": key,
                        }
                    },
                },
            )
            seen_secret.add(name)
    return out


def parse_user(user):
    if user is None:
        return None
    text = str(user)
    if ":" in text:
        uid, _, gid = text.partition(":")
    else:
        uid, gid = text, None
    try:
        sc = {"runAsUser": int(uid)}
        if gid:
            sc["runAsGroup"] = int(gid)
        return sc
    except ValueError:
        return None


def parse_port_mappings(ports):
    """Return the container ports a Compose `ports:` list maps to."""
    out = []
    for p in ports or []:
        text = str(p)
        text = text.split("/")[0]
        parts = text.split(":")
        target = parts[1] if len(parts) >= 2 else parts[0]
        if target.isdigit():
            out.append(int(target))
    return out


def service_port(name, svc):
    ports = parse_port_mappings(svc.get("ports"))
    if ports:
        return ports[0]
    image = str(svc.get("image") or "")
    base = image.split("@")[0].rsplit("/", 1)[-1].split(":")[0].lower()
    for key, port in DEFAULT_PORTS.items():
        if base == key or base.startswith(key):
            return port
    return None


def probe_from_healthcheck(hc):
    if not hc:
        return None
    test = hc.get("test")
    cmd = None
    if isinstance(test, str):
        cmd = ["/bin/sh", "-c", test]
    elif isinstance(test, list) and test:
        if test[0] == "CMD":
            cmd = [str(x) for x in test[1:]]
        elif test[0] == "CMD-SHELL":
            cmd = ["/bin/sh", "-c", " ".join(str(x) for x in test[1:])]
        elif test[0] == "NONE":
            return None
    if not cmd:
        return None
    probe = {"exec": {"command": cmd}}
    if hc.get("start_period"):
        probe["initialDelaySeconds"] = parse_duration(hc["start_period"])
    if hc.get("interval"):
        probe["periodSeconds"] = parse_duration(hc["interval"])
    return probe


def parse_duration(text):
    m = re.match(r"^(\d+)([smhd]?)", str(text))
    if not m:
        return None
    n = int(m.group(1))
    unit = m.group(2)
    if unit == "m":
        return n * 60
    if unit == "h":
        return n * 3600
    if unit == "d":
        return n * 86400
    return n


def container_volume_mounts(svc, app_id):
    """Turn Compose volumes into volumeMounts, splitting app data from the host.

    Umbrel's `${APP_DATA_DIR}` is this app's own directory, which is exactly
    what the chart's raw PVC is, so `/<rel>` under the PVC mirrors it.
    `${UMBREL_ROOT}` storage and named volumes also land on the PVC. Anything
    the Compose file takes straight from the host (`/dev`, `/run/dbus`, the
    Docker socket, a USB stick) becomes a hostPath volume, because those apps
    exist precisely to talk to the machine they run on.
    """
    mounts = []
    host_volumes = []

    def add_host(path, target, read_only):
        name = "host-" + re.sub(r"[^a-z0-9]+", "-", path.strip("/").lower()).strip("-")
        taken = {v["name"] for v in host_volumes}
        base, n = name, 1
        while name in taken:
            n += 1
            name = f"{base}-{n}"
        volume = {"name": name, "hostPath": {"path": path}}
        mount = {"name": name, "mountPath": target}
        if read_only:
            mount["readOnly"] = True
        host_volumes.append(volume)
        mounts.append(mount)

    for v in svc.get("volumes") or []:
        if not isinstance(v, str):
            continue
        parts = v.split(":")
        src = parts[0]
        target = parts[1] if len(parts) >= 2 else None
        mode = parts[2] if len(parts) >= 3 else ""
        if not target:
            continue
        read_only = "ro" in mode.split(",")
        if src.startswith("${APP_DATA_DIR}"):
            mount = {"name": "data", "mountPath": target}
            rel = src[len("${APP_DATA_DIR}") :].lstrip("/")
            if rel:
                mount["subPath"] = "{{ .Release.Name }}/" + rel
            if read_only:
                mount["readOnly"] = True
            mounts.append(mount)
        elif src.startswith("${UMBREL_ROOT}"):
            mount = {"name": "data", "mountPath": target}
            rel = src[len("${UMBREL_ROOT}") :].lstrip("/")
            if rel.startswith("data/storage"):
                rem = rel[len("data/storage") :].lstrip("/")
                mount["subPath"] = "{{ .Release.Name }}/storage" + (
                    ("/" + rem) if rem else ""
                )
            else:
                mount["subPath"] = "{{ .Release.Name }}/umbrel/" + rel
            if read_only:
                mount["readOnly"] = True
            mounts.append(mount)
        elif src.startswith("/"):
            add_host(src, target, read_only)
        else:
            mount = {"name": "data", "mountPath": target}
            mount["subPath"] = "{{ .Release.Name }}/volumes/" + src.replace("/", "-")
            if read_only:
                mount["readOnly"] = True
            mounts.append(mount)

    for dev in svc.get("devices") or []:
        text = str(dev)
        parts = text.split(":")
        host = parts[0]
        target = parts[1] if len(parts) >= 2 and parts[1] else host
        if host.startswith("/"):
            add_host(host, target, False)

    return mounts, host_volumes


def build_container(
    name, svc, app_id, secrets, ports, env_file_vars=None, force_command=None
):
    c = {"name": name, "image": svc["image"], "imagePullPolicy": "IfNotPresent"}
    if force_command:
        c["command"] = force_command
    entrypoint = svc.get("entrypoint")
    command = svc.get("command")
    if entrypoint is not None:
        c["command"] = [own_hostnames(a, app_id) for a in compose_cmd(entrypoint)]
    if command is not None:
        c["args"] = [own_hostnames(a, app_id) for a in compose_cmd(command)]
    # env_file first: Compose lets `environment` override it.
    merged = list(env_file_vars or []) + env_items(svc.get("environment"))
    env = k8s_env(merged, app_id, secrets)
    if env:
        c["env"] = env
    sc = parse_user(svc.get("user")) or {}
    caps = [str(x) for x in (svc.get("cap_add") or [])]
    if caps:
        sc["capabilities"] = {"add": caps}
    if sc:
        c["securityContext"] = sc
    if ports:
        c["ports"] = [{"containerPort": p} for p in ports]
    probe = probe_from_healthcheck(svc.get("healthcheck"))
    if probe:
        c["readinessProbe"] = probe
    elif ports:
        c["readinessProbe"] = {"tcpSocket": {"port": ports[0]}}
    mounts, host_volumes = container_volume_mounts(svc, app_id)
    if mounts:
        c["volumeMounts"] = mounts
    if host_volumes:
        c["_hostVolumes"] = host_volumes
    return c


BUSYBOX = "busybox:1.37@sha256:9db7b59979c38555a39def84a31fb98b5296952f9e3afd4f6f11f05b07adfab0"


def appdata_paths(compose):
    """Relative paths the Compose file takes from `${APP_DATA_DIR}`."""
    found = set()
    for svc in (compose.get("services") or {}).values():
        svc = svc or {}
        for v in svc.get("volumes") or []:
            if isinstance(v, str) and v.startswith("${APP_DATA_DIR}"):
                found.add(v.split(":", 1)[0][len("${APP_DATA_DIR}") :].lstrip("/"))
        for ef in svc.get("env_file") or []:
            text = str(ef)
            if text.startswith("${APP_DATA_DIR}"):
                found.add(text[len("${APP_DATA_DIR}") :].lstrip("/"))
    return found


def collect_seeds(src_dir, app_id, compose):
    """Files Umbrel seeds into APP_DATA_DIR from the app package.

    Umbrel copies the package's `data/` directory to `${APP_DATA_DIR}/data`
    and leaves the rest of the package at `${APP_DATA_DIR}` itself, which is
    how apps ship default config (`settings.env`, `config/nginx.conf`). The
    chart reproduces both with a ConfigMap and an init container that copies
    each file in only if it is not already there, so an update never clobbers
    what the user changed.
    """
    root = os.path.join(src_dir, app_id)
    seeds = []
    total = 0

    def add(dest, path):
        nonlocal total
        if not os.path.exists(path) or os.path.islink(path):
            return True
        if os.path.isdir(path):
            for r, _, fs in os.walk(path):
                for fname in fs:
                    if fname == ".gitkeep":
                        continue
                    rel = os.path.relpath(os.path.join(r, fname), path)
                    if not add(
                        os.path.join(dest, rel).replace("\\", "/"),
                        os.path.join(r, fname),
                    ):
                        return False
            return True
        if os.path.basename(path) == ".gitkeep":
            return True
        with open(path, "rb") as f:
            blob = f.read()
        total += len(blob)
        seeds.append((dest, blob))
        return True

    data_dir = os.path.join(root, "data")
    if os.path.isdir(data_dir) and not add("data", data_dir):
        return None
    for rel in sorted(appdata_paths(compose)):
        if not rel or rel == "data" or rel.startswith("data/"):
            continue
        add(rel, os.path.join(root, rel))

    if total > 900 * 1024 or any(len(b) > 200 * 1024 for _, b in seeds):
        return None
    return sorted(seeds)


def parse_env_file(text):
    pairs = []
    for line in text.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if line.startswith("export "):
            line = line.removeprefix("export ")
        key, sep, value = line.partition("=")
        if not sep:
            continue
        pairs.append((key.strip(), value.strip().strip('"').strip("'")))
    return pairs


def env_file_vars(src_dir, app_id, svc):
    """Inline a Compose `env_file` the way Compose would export it."""
    pairs = []
    for ef in svc.get("env_file") or []:
        text = str(ef)
        if text.startswith("${APP_DATA_DIR}"):
            rel = text[len("${APP_DATA_DIR}") :].lstrip("/")
        elif text.startswith("/"):
            continue
        else:
            rel = text
        path = os.path.join(src_dir, app_id, rel)
        if os.path.isfile(path):
            with open(path) as f:
                pairs.extend(parse_env_file(f.read()))
    return pairs


def seed_script(seeds):
    lines = ["set -e", "D=/data/{{ .Release.Name }}"]
    for i, (dest, _) in enumerate(seeds):
        parent = os.path.dirname(dest)
        if parent:
            lines.append(f'mkdir -p "$D/{parent}"')
        lines.append(f'[ -e "$D/{dest}" ] || cp /seed/f{i} "$D/{dest}"')
    return "\n".join(lines) + "\n"


def seed_init_container(seeds):
    return {
        "name": "seed",
        "image": BUSYBOX,
        "imagePullPolicy": "IfNotPresent",
        "command": ["/bin/sh", "-c", seed_script(seeds)],
        "volumeMounts": [
            {"name": "seed", "mountPath": "/seed", "readOnly": True},
            {"name": "data", "mountPath": "/data"},
        ],
    }


def seed_configmap(seeds):
    import base64

    return {
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": {
            "name": "{{ .Release.Name }}-seed",
            "namespace": "{{ .Release.Namespace }}",
        },
        "binaryData": {
            f"f{i}": base64.b64encode(blob).decode()
            for i, (_, blob) in enumerate(seeds)
        },
    }


def compose_cmd(value):
    """Compose `command`/`entrypoint` (string or list) -> a k8s argv list."""
    if isinstance(value, list):
        return [str(x) for x in value]
    text = str(value)
    if re.search(r"[|&;<>()`$]", text):
        return ["/bin/sh", "-c", text]
    try:
        return shlex.split(text)
    except ValueError:
        return ["/bin/sh", "-c", text]


# ---------------------------------------------------------------------------
# Schema
# ---------------------------------------------------------------------------


def explorer_password_prop():
    return {
        "type": "string",
        "title": "File explorer password",
        "minLength": 12,
        "writeOnly": True,
        "generate": True,
    }


def build_schema(name, display, has_app_secret, has_admin_password, offer_node=False):
    config = {
        "yolab_enabled": {
            "type": "boolean",
            "title": "YoLab address",
            "default": True,
            "description": "Reachable from anywhere at https://<subdomain>.<your-number>.yolab.io, through your YoLab tunnel. Visitors need IPv6.",
        },
        "storage_size": {
            "type": "string",
            "title": "Storage size",
            "default": "10Gi",
            "description": "How much CephFS storage to allocate (e.g. 20Gi, 100Gi, 500Gi)",
        },
    }
    if offer_node:
        config["node"] = {
            "type": "string",
            "title": "Machine",
            "default": "",
            "description": "This app joins the machine's own network to reach devices on your LAN, so it can only run on one machine at a time. Name that machine here (the hostname shown in the fleet list); leave empty to let YoLab choose. Two apps doing this cannot share a machine unless their ports differ.",
        }
    if has_app_secret:
        config["app_secret"] = {
            "type": "string",
            "title": "App secret",
            "writeOnly": True,
            "generate": True,
            "description": "Signing key generated for this app. Rotating it signs everyone out.",
        }
    if has_admin_password:
        config["admin_password"] = {
            "type": "string",
            "title": "Admin password",
            "minLength": 12,
            "writeOnly": True,
            "generate": True,
        }
    config["file_explorer_enabled"] = {
        "type": "boolean",
        "title": "File explorer",
        "default": False,
        "description": "A password-protected file manager for this app's own data on its own address: browse, download, upload, rename and delete files without needing a terminal. Folders holding a database server (Postgres, MariaDB, Redis, MongoDB, MinIO) always stay read-only.",
    }

    config_deps = {
        "yolab_enabled": {
            "oneOf": [
                {
                    "properties": {"yolab_enabled": {"const": False}},
                    "required": ["yolab_enabled"],
                },
                {
                    "properties": {
                        "yolab_enabled": {"const": True},
                        "subdomain": {
                            "type": "string",
                            "title": "Subdomain",
                            "format": "tunnel",
                            "default": name,
                        },
                        "yolab_token": {
                            "type": "string",
                            "title": "YoLab token",
                            "format": "yolab-token",
                            "writeOnly": True,
                            "description": "Leave it as it is to use this box's YoLab account.",
                        },
                    },
                    "required": ["subdomain"],
                },
            ]
        },
        "file_explorer_enabled": {
            "oneOf": [
                {"properties": {"file_explorer_enabled": {"const": False}}},
                {
                    "properties": {
                        "file_explorer_enabled": {"const": True},
                        "file_explorer_yolab_enabled": {
                            "type": "boolean",
                            "title": "File explorer YoLab address",
                            "default": True,
                            "description": "The file explorer reachable from anywhere at https://<subdomain>.<your-number>.yolab.io, through your YoLab tunnel. Visitors need IPv6.",
                        },
                        "file_explorer_tor_enabled": {
                            "type": "boolean",
                            "title": "File explorer on Tor",
                            "default": False,
                            "description": "The file explorer also reachable as its own .onion address in Tor Browser, from any network and without IPv6. Slower, and visitors never learn your home's IP address.",
                        },
                        "file_explorer_tailscale_enabled": {
                            "type": "boolean",
                            "title": "File explorer on Tailscale",
                            "default": False,
                            "description": "The file explorer also reachable from your own devices in your Tailscale network, as its own device at https://<name>.<your-tailnet>.ts.net. Turn on HTTPS certificates in the Tailscale admin console (DNS page) first.",
                        },
                        "file_explorer_username": {
                            "type": "string",
                            "title": "File explorer username",
                            "default": "admin",
                            "pattern": "^[A-Za-z0-9._-]+$",
                        },
                        "file_explorer_password": explorer_password_prop(),
                        "file_explorer_read_only": {
                            "type": "boolean",
                            "title": "Read only",
                            "default": False,
                            "description": "Only browse and download. Nothing can be uploaded, renamed, edited or deleted through the file explorer.",
                        },
                    },
                    "dependencies": {
                        "file_explorer_yolab_enabled": {
                            "oneOf": [
                                {
                                    "properties": {
                                        "file_explorer_yolab_enabled": {"const": False}
                                    }
                                },
                                {
                                    "properties": {
                                        "file_explorer_yolab_enabled": {"const": True},
                                        "file_explorer_subdomain": {
                                            "type": "string",
                                            "title": "File explorer subdomain",
                                            "format": "tunnel",
                                            "description": "Leave empty to use this app's subdomain followed by -files.",
                                        },
                                    },
                                },
                            ]
                        },
                        "file_explorer_tailscale_enabled": {
                            "oneOf": [
                                {
                                    "properties": {
                                        "file_explorer_tailscale_enabled": {
                                            "const": False
                                        }
                                    }
                                },
                                {
                                    "properties": {
                                        "file_explorer_tailscale_enabled": {
                                            "const": True
                                        },
                                        "file_explorer_tailscale_auth_key": {
                                            "type": "string",
                                            "title": "File explorer Tailscale auth key",
                                            "writeOnly": True,
                                            "pattern": "^tskey-",
                                            "description": "Create one in the Tailscale admin console under Settings, Keys. A reusable key can be the same one the app uses. It is used only the first time.",
                                        },
                                        "file_explorer_tailscale_hostname": {
                                            "type": "string",
                                            "title": "File explorer Tailscale name",
                                            "pattern": "^[a-z0-9]([a-z0-9-]*[a-z0-9])?$",
                                            "description": "The file explorer's device name in your tailnet. Leave empty to use its subdomain.",
                                        },
                                    },
                                    "required": ["file_explorer_tailscale_auth_key"],
                                },
                            ]
                        },
                    },
                },
            ]
        },
    }

    on = {"properties": {"yolab_enabled": {"const": True}}}
    on_explorer = {"properties": {"file_explorer_enabled": {"const": True}}}
    outputs = {
        "url": {
            "type": "string",
            "title": "Web URL",
            "format": "uri",
            "source": {"logs": "YOLAB_OUTPUT url (\\S+)"},
            "when": on,
        },
        "file_explorer_url": {
            "type": "string",
            "title": "File explorer",
            "format": "uri",
            "source": {"logs": "YOLAB_OUTPUT file_explorer_url (\\S+)"},
            "when": {
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
            },
        },
        "file_explorer_username": {
            "type": "string",
            "title": "File explorer username",
            "source": {"logs": "YOLAB_OUTPUT file_explorer_username (\\S+)"},
            "when": on_explorer,
        },
        "file_explorer_password": {
            "type": "string",
            "title": "File explorer password",
            "format": "secret",
            "source": {"logs": "YOLAB_OUTPUT file_explorer_password (\\S+)"},
            "when": on_explorer,
        },
        "file_explorer_tor_url": {
            "type": "string",
            "title": "File explorer Tor address",
            "format": "uri",
            "source": {"logs": "YOLAB_OUTPUT file_explorer_tor_url (\\S+)"},
            "when": {
                "properties": {
                    "file_explorer_enabled": {"const": True},
                    "file_explorer_tor_enabled": {"const": True},
                }
            },
        },
        "file_explorer_tailscale_url": {
            "type": "string",
            "title": "File explorer Tailscale address",
            "format": "uri",
            "source": {"logs": "YOLAB_OUTPUT file_explorer_tailscale_url (\\S+)"},
            "when": {
                "properties": {
                    "file_explorer_enabled": {"const": True},
                    "file_explorer_tailscale_enabled": {"const": True},
                }
            },
        },
    }

    return {
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "type": "object",
        "required": ["config"],
        "properties": {
            "config": {
                "title": display,
                "type": "object",
                "properties": config,
                "dependencies": config_deps,
            },
            "outputs": {
                "type": "object",
                "readOnly": True,
                "properties": outputs,
            },
        },
    }


# ---------------------------------------------------------------------------
# Emit the chart
# ---------------------------------------------------------------------------


def yaml_block(obj, indent=0):
    text = yaml.safe_dump(obj, sort_keys=False, default_flow_style=False, width=1000)
    return textwrap.indent(text, " " * indent)


def gateway_primary_block(container):
    return yaml_block([container], 6)


def dep_deployment(
    name, container, release_volume=True, seeds=(), node_amd64=False, host_network=False
):
    host_volumes = container.pop("_hostVolumes", [])
    volumes = []
    if release_volume:
        volumes.append(
            {
                "name": "data",
                "persistentVolumeClaim": {
                    "claimName": "{{ .Release.Name }}-data",
                },
            }
        )
    volumes.extend(host_volumes)
    init_containers = []
    if seeds:
        init_containers.append(seed_init_container(seeds))
        volumes.append(
            {"name": "seed", "configMap": {"name": "{{ .Release.Name }}-seed"}}
        )
    spec = {"containers": [container], "volumes": volumes}
    if init_containers:
        spec["initContainers"] = init_containers
    if host_network:
        spec["hostNetwork"] = True
        spec["dnsPolicy"] = "ClusterFirstWithHostNet"
    if node_amd64:
        spec["nodeSelector"] = {"kubernetes.io/arch": "amd64"}
    return {
        "apiVersion": "apps/v1",
        "kind": "Deployment",
        "metadata": {"name": name, "namespace": "{{ .Release.Namespace }}"},
        "spec": {
            "replicas": 1,
            "strategy": {"type": "Recreate"},
            "selector": {"matchLabels": {"app": name}},
            "template": {
                "metadata": {"labels": {"app": name}},
                "spec": spec,
            },
        },
    }


def dep_service(name, ports):
    return {
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {"name": name, "namespace": "{{ .Release.Namespace }}"},
        "spec": {
            "selector": {"app": name},
            "ports": [{"port": p, "targetPort": p} for p in ports],
        },
    }


def build_chart(app_id, um, compose, out_dir, src_dir, seeds=()):
    svcs = compose.get("services") or {}
    proxy = svcs.get("app_proxy") or {}
    proxy_env = dict(env_items(proxy.get("environment")))
    app_host = proxy_env.get("APP_HOST", "")
    m = re.match(r"^.*?_([a-z0-9][a-z0-9_-]*)_1$", app_host)
    if m and m.group(1) in svcs:
        primary_name = m.group(1)
    else:
        primary_name = next((s for s in svcs if s != "app_proxy"), None)
    app_port = None
    if proxy_env.get("APP_PORT"):
        try:
            app_port = int(str(proxy_env["APP_PORT"]).split("/")[0])
        except ValueError:
            app_port = None
    if app_port is None:
        app_port = service_port(primary_name, svcs.get(primary_name) or {})
    if app_port is None and um.get("port"):
        try:
            app_port = int(str(um["port"]).split("/")[0])
        except ValueError:
            app_port = None
    if app_port is None:
        app_port = 8080

    host_services = {
        s for s, sv in svcs.items() if (sv or {}).get("network_mode") == "host"
    }
    host_net = primary_name in host_services
    offer_node = bool(host_services)

    secrets = set()
    # Decide whether the primary can share the gateway pod. Ports 80/443 collide
    # with Caddy there, and so does anything the check would flag; when that
    # happens the app moves to its own Deployment behind a Service.
    share_gateway = app_port not in (80, 443)

    # Build every service container once (env rewriting populates `secrets`).
    containers = {}
    for sname, svc in svcs.items():
        if sname == "app_proxy":
            continue
        port = app_port if sname == primary_name else service_port(sname, svc or {})
        ports = [port] if port else []
        efv = env_file_vars(src_dir, app_id, svc or {})
        containers[sname] = (
            build_container(sname, svc or {}, app_id, secrets, ports, efv),
            ports,
        )

    has_app_secret = "APP_SEED" in secrets
    has_admin_password = "APP_PASSWORD" in secrets

    desc = (um.get("tagline") or um.get("name") or app_id).strip().replace('"', "'")
    home = um.get("website") or um.get("repo") or ""
    tagline = desc
    if len(tagline) > 60:
        tagline = tagline[:60].rsplit(" ", 1)[0].rstrip(",.;:")
    display = um.get("name") or app_id
    category = str(um.get("category") or "").lower()
    icon = CATEGORY_ICON.get(category, "\U0001f9e9")
    github = None
    repo = str(um.get("repo") or "")
    gm = re.search(r"github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)", repo) or re.search(
        r"github\.com/([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)", str(um.get("website") or "")
    )
    if gm:
        github = gm.group(1).removesuffix(".git")

    annotations = {
        "yolab.io/display-name": display,
        "yolab.io/icon": icon,
        "yolab.io/category": category or "utilities",
        "yolab.io/tagline": tagline,
    }
    if github:
        annotations["yolab.io/github"] = github
    if category in COLLECTIONS:
        annotations["yolab.io/collections"] = COLLECTIONS[category]

    chart_yaml = {
        "apiVersion": "v2",
        "name": app_id,
        "description": desc,
        "type": "application",
        "version": "0.1.0",
        "appVersion": str(um.get("version") or "1"),
        "dependencies": [
            {
                "name": "yolab-common",
                "version": LIB_VERSION,
                "repository": "oci://ghcr.io/demycode/charts",
            }
        ],
        "annotations": annotations,
    }
    if home:
        chart_yaml["home"] = home

    # Secret + protect values
    secret_entries = []
    if has_app_secret:
        secret_entries.append(("app_secret", "config.app_secret"))
    if has_admin_password:
        secret_entries.append(("admin_password", "config.admin_password"))
    protect = []
    for sname, (c, ports) in containers.items():
        base = c["image"].split("@")[0].rsplit("/", 1)[-1].split(":")[0].lower()
        if base in DEFAULT_PORTS:
            for mount in c.get("volumeMounts", []):
                sub = mount.get("subPath", "")
                if sub.startswith("{{ .Release.Name }}/"):
                    rel = sub[len("{{ .Release.Name }}/") :]
                    if rel and rel not in protect:
                        protect.append(rel)

    values = {
        "config": {"subdomain": app_id, "storage_size": "10Gi"},
        "yolab": {
            "platformApiUrl": "",
            "accountToken": "",
            "serviceName": "",
            "gateway": {
                "upstream": f"localhost:{app_port}"
                if share_gateway
                else f"{primary_name}:{app_port}"
            },
        },
    }
    if offer_node:
        values["config"]["node"] = ""
    if protect:
        values["yolab"]["fileExplorer"] = {"protect": sorted(protect)}

    schema = build_schema(
        app_id, display, has_app_secret, has_admin_password, offer_node=offer_node
    )

    # ---- templates/app.yaml ----
    primary_container, _ = containers[primary_name]
    primary_host_volumes = primary_container.pop("_hostVolumes", [])
    gw_arch = needs_amd64(
        ([primary_container["image"]] if share_gateway else [])
        + ([BUSYBOX] if seeds else [])
    )
    parts = [
        '{{ include "yolab-common.caddyConfigMap" . }}\n---\n'
        + '{{ include "yolab-common.fileExplorerSecret" . }}\n---\n'
        + '{{ include "yolab-common.fileExplorerConfigMap" . }}\n---\n'
        + '{{ include "yolab-common.privateAccessResources" . }}\n---\n'
        + '{{ include "yolab-common.uninstallHook" . }}\n---\n'
    ]
    parts.append(
        "apiVersion: v1\nkind: PersistentVolumeClaim\n"
        "metadata:\n  name: {{ .Release.Name }}-data\n  namespace: {{ .Release.Namespace }}\n"
        "spec:\n  accessModes:\n    - ReadWriteMany\n  storageClassName: yolab-cephfs\n"
        '  resources:\n    requests:\n      storage: {{ .Values.config.storage_size | default "10Gi" | quote }}\n'
    )
    if secret_entries:
        parts.append("---\n")
        secret = ["apiVersion: v1\nkind: Secret\n"]
        secret.append(
            "metadata:\n  name: {{ .Release.Name }}-secrets\n  namespace: {{ .Release.Namespace }}\n"
        )
        secret.append("type: Opaque\nstringData:\n")
        for key, val in secret_entries:
            secret.append(
                f'  {key}: {{{{ required "{val} is required" .Values.{val} | quote }}}}\n'
            )
        parts.append("".join(secret))
    if seeds:
        parts.append("---\n")
        parts.append(yaml.safe_dump(seed_configmap(seeds), sort_keys=False))

    # gateway Deployment
    gw = [
        "---\n",
        "apiVersion: apps/v1\nkind: Deployment\n",
        "metadata:\n  name: gateway\n  namespace: {{ .Release.Namespace }}\n",
        "spec:\n  replicas: 1\n  strategy:\n    type: Recreate\n",
        "  selector:\n    matchLabels:\n      app: gateway\n",
        "  template:\n    metadata:\n      labels:\n        app: gateway\n",
        "    spec:\n",
    ]
    if host_net:
        gw.append("      hostNetwork: true\n")
        gw.append("      dnsPolicy: ClusterFirstWithHostNet\n")
    if offer_node:
        gw.append(
            "      {{- if .Values.config.node }}\n"
            "      nodeSelector:\n"
            "        kubernetes.io/hostname: {{ .Values.config.node | quote }}\n"
            + ("        kubernetes.io/arch: amd64\n" if gw_arch else "")
            + "      {{- else }}\n"
            + (
                "      nodeSelector:\n        kubernetes.io/arch: amd64\n"
                if gw_arch
                else ""
            )
            + "      {{- end }}\n"
        )
    elif gw_arch:
        gw.append("      nodeSelector:\n        kubernetes.io/arch: amd64\n")
    gw += [
        "      initContainers:\n",
        '      {{- include "yolab-common.wgRegisterInit" . | nindent 6 }}\n',
        '      {{- include "yolab-common.fileExplorerInit" . | nindent 6 }}\n',
        '      {{- include "yolab-common.privateAccessInit" . | nindent 6 }}\n',
    ]
    if seeds:
        gw.append(yaml_block([seed_init_container(seeds)], 6))
    gw += [
        "      containers:\n",
        '      {{- include "yolab-common.gatewayContainers" . | nindent 6 }}\n',
        '      {{- include "yolab-common.fileExplorerContainer" . | nindent 6 }}\n',
        '      {{- include "yolab-common.privateAccessContainers" . | nindent 6 }}\n',
    ]
    if share_gateway:
        gw.append(gateway_primary_block(primary_container))
    gw.append("      volumes:\n")
    gw.append('      {{- include "yolab-common.gatewayVolumes" . | nindent 6 }}\n')
    gw.append('      {{- include "yolab-common.fileExplorerVolumes" . | nindent 6 }}\n')
    gw.append(
        '      {{- include "yolab-common.privateAccessVolumes" . | nindent 6 }}\n'
    )
    if seeds:
        gw.append(
            yaml_block(
                [{"name": "seed", "configMap": {"name": "{{ .Release.Name }}-seed"}}], 6
            )
        )
    if primary_host_volumes:
        gw.append(yaml_block(primary_host_volumes, 6))
    parts.append("".join(gw))

    # dependencies + the primary when it cannot share the gateway
    for sname, (c, ports) in containers.items():
        if sname == primary_name and share_gateway:
            continue
        images = [c["image"]] + ([BUSYBOX] if seeds else [])
        parts.append("---\n")
        parts.append(
            yaml.safe_dump(
                dep_deployment(
                    sname,
                    c,
                    release_volume=True,
                    seeds=seeds,
                    node_amd64=needs_amd64(images),
                    host_network=sname in host_services,
                ),
                sort_keys=False,
            )
        )
        if ports:
            parts.append("---\n")
            parts.append(yaml.safe_dump(dep_service(sname, ports), sort_keys=False))

    app_yaml = "".join(parts)

    previous = os.path.join(out_dir, "Chart.yaml")
    if os.path.isfile(previous):
        chart_yaml = carry_over(load_yaml(previous), chart_yaml)
    os.makedirs(os.path.join(out_dir, "templates"), exist_ok=True)
    with open(os.path.join(out_dir, "Chart.yaml"), "w") as f:
        yaml.safe_dump(chart_yaml, f, sort_keys=False, allow_unicode=True, width=1000)
    with open(os.path.join(out_dir, "values.yaml"), "w") as f:
        yaml.safe_dump(values, f, sort_keys=False, allow_unicode=True, width=1000)
    with open(os.path.join(out_dir, "values.schema.json"), "w") as f:
        json.dump(schema, f, indent=2)
        f.write("\n")
    with open(os.path.join(out_dir, "templates", "app.yaml"), "w") as f:
        f.write(app_yaml)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--src", default=DEFAULT_SRC)
    ap.add_argument("--out", default=HERE)
    ap.add_argument("--only", nargs="*", default=None)
    ap.add_argument("--force", action="store_true")
    args = ap.parse_args()

    existing = {
        d
        for d in os.listdir(args.out)
        if d != "yolab-common"
        and os.path.isfile(os.path.join(args.out, d, "Chart.yaml"))
    }
    apps = sorted(
        d
        for d in os.listdir(args.src)
        if os.path.isfile(os.path.join(args.src, d, "docker-compose.yml"))
    )
    if args.only:
        apps = [a for a in apps if a in args.only]

    converted, skipped = [], []
    for app_id in apps:
        if (
            args.only is None
            and (app_id in existing or ALIAS.get(app_id) in existing)
            and not args.force
        ):
            skipped.append((app_id, "already in the catalogue"))
            continue
        try:
            compose = load_yaml(os.path.join(args.src, app_id, "docker-compose.yml"))
            um = load_yaml(os.path.join(args.src, app_id, "umbrel-app.yml"))
        except Exception as e:  # noqa: BLE001
            skipped.append((app_id, f"unreadable: {e}"))
            continue
        reason = skip_reason(
            app_id,
            compose,
            existing - {app_id, ALIAS.get(app_id)} if args.force else existing,
        )
        if reason:
            skipped.append((app_id, reason))
            continue
        out_dir = os.path.join(args.out, app_id)
        if os.path.isdir(out_dir) and not args.force:
            skipped.append((app_id, "output exists"))
            continue
        seeds = collect_seeds(args.src, app_id, compose)
        if seeds is None:
            skipped.append((app_id, "seed files too large to ship in a ConfigMap"))
            continue
        try:
            build_chart(app_id, um, compose, out_dir, args.src, seeds)
            converted.append(app_id)
        except Exception as e:  # noqa: BLE001
            skipped.append((app_id, f"conversion failed: {e}"))
            if os.path.isdir(out_dir):
                shutil.rmtree(out_dir)

    log(f"converted {len(converted)} apps")
    for app_id, reason in skipped:
        log(f"  skip {app_id}: {reason}")


if __name__ == "__main__":
    main()
