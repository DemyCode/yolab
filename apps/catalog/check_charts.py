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

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
LIBRARY = os.path.join(HERE, "yolab-common")

LINT_VALUES = {
    "config.password": "PlaceholderPw2026",
    "config.admin_password": "PlaceholderPw2026",
    "config.admin_email": "admin@example.com",
    "config.app_secret": "PlaceholderPw2026",
    "config.app_key": "PlaceholderAppKey2026Placeholder",
    "config.api_key": "PlaceholderApiKey2026",
    "config.auth_secret_key": "PlaceholderAuthSecretKey2026Placeholder",
    "config.gateway_token": "PlaceholderGatewayToken2026Placeholder",
    "config.server_name": "example",
    "config.server_pass": "PlaceholderPw2026",
    "config.subdomain": "example",
    "config.vpn_private_key": "PlaceholderVpnKey2026=",
    "config.vpn_addresses": "10.64.0.2/32",
    "config.auth_users[0].username": "admin",
    "config.auth_users[0].password": "PlaceholderPw2026",
}

GATEWAY_CONTAINERS = ("wireguard", "caddy")
TOKEN_CONTAINERS = ("wg-register", "cleanup")


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


def check(app, docs, fail, chart_yaml="", schema=None):
    kinds = {}
    for d in docs:
        kinds.setdefault(d["kind"], []).append(d)

    has_caddy = any(
        "Caddyfile" in (c.get("data") or {}) for c in kinds.get("ConfigMap", [])
    )

    for kind, want in (("PersistentVolumeClaim", 1), ("Job", 1)):
        got = len(kinds.get(kind, []))
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
    if len(tunnel_pods) != 1:
        fail(
            app,
            f"expected exactly one pod running wg-register, found {len(tunnel_pods)}",
        )
        return
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

    pod_labels = [
        tuple(sorted(d["spec"]["template"]["metadata"]["labels"].items()))
        for d in deploys.values()
    ]
    for s in kinds.get("Service", []):
        sel = tuple(sorted(s["spec"]["selector"].items()))
        if not any(all(kv in lbl for kv in sel) for lbl in pod_labels):
            fail(
                app,
                f"Service {s['metadata']['name']} selector {dict(sel)} matches no pod",
            )

    for d in list(deploys.values()) + kinds.get("Job", []):
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
        for key in ("file_explorer_url", "file_explorer_password"):
            if key not in declared:
                fail(
                    app,
                    f"renders the file explorer but its schema's outputs "
                    f"do not declare {key}",
                )
            elif declared[key].get("when") != EXPLORER_ON:
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


def claim_mounts(spec, container):
    claims = {
        v["name"]: v["persistentVolumeClaim"]["claimName"]
        for v in spec.get("volumes") or []
        if "persistentVolumeClaim" in v
    }
    for m in container.get("volumeMounts") or []:
        if m["name"] in claims:
            yield claims[m["name"]], m.get("subPath", ""), m.get("readOnly") is True


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
        fail(app, f"pod {pod_name} runs file-explorer-init but no file-explorer container")
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
            fail(app, f"read-only file-explorer leaves source {source.get('name')} writable")
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
OUTPUT_FORMATS = {"text", "uri", "secret", "multiline"}
LEGACY_ANNOTATIONS = ("yolab.io/uischema", "yolab.io/outputs")


def check_schema(app, schema, chart_yaml, fail):
    try:
        annotations = (yaml.safe_load(chart_yaml) or {}).get("annotations") or {}
    except yaml.YAMLError:
        annotations = {}
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

    config = (props.get("config") or {}).get("properties") or {}
    tunnels = [n for n, p in config.items() if p.get("format") == "tunnel"]
    if len(tunnels) > 1:
        fail(app, f"more than one address field: {', '.join(tunnels)}")
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
            check(app, docs, fail, text, schema)
            check_file_explorer(app, docs, fail)

            if explorer_pod(docs)[1] is None:
                continue
            rendered, err = render(
                chart_dir,
                library_tgz,
                tmp,
                {"config.file_explorer_read_only": "true"},
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

    print(f"checked {len(chart_dirs)} charts")
    for f in fail.items:
        print("FAIL " + f)
    print(f"FAILURES: {len(fail)}" if len(fail) else "all assertions passed")
    return 1 if len(fail) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
