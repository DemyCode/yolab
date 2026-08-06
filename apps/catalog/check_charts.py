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

import yaml

HERE = os.path.dirname(os.path.abspath(__file__))
LIBRARY = os.path.join(HERE, "yolab-common")

# Charts whose schema marks fields required cannot render from defaults alone.
# Mirrors the --set list in .github/workflows/push.yml; keep the two in step, or
# a chart passes here and fails to publish there.
LINT_VALUES = {
    "config.password": "PlaceholderPw2026",
    "config.admin_password": "PlaceholderPw2026",
    "config.admin_email": "admin@example.com",
    "config.app_secret": "PlaceholderPw2026",
    # Exactly 32 characters: Firefly III's schema pins minLength and maxLength there,
    # and helm validates values.schema.json during template.
    "config.app_key": "PlaceholderAppKey2026Placeholder",
    "config.api_key": "PlaceholderApiKey2026",
    "config.auth_secret_key": "PlaceholderAuthSecretKey2026Placeholder",
    "config.gateway_token": "PlaceholderGatewayToken2026Placeholder",
    "config.server_name": "example",
    "config.server_pass": "PlaceholderPw2026",
    "config.subdomain": "example",
}

# Containers the platform injects into the gateway pod, as opposed to the app's own.
GATEWAY_CONTAINERS = ("wireguard", "caddy")
# The only containers allowed to see the platform account token.
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


def render(chart_dir, library_tgz, workdir):
    """helm template the chart against the local library. Returns the YAML text.

    Renders from a copy: the chart has to gain a `charts/` directory holding the
    library, and the source tree is read-only when this runs from the nix store.
    """
    staged = os.path.join(workdir, os.path.basename(chart_dir.rstrip("/")))
    shutil.copytree(chart_dir, staged, dirs_exist_ok=True)
    os.makedirs(os.path.join(staged, "charts"), exist_ok=True)
    shutil.copy(library_tgz, os.path.join(staged, "charts"))

    cmd = ["helm", "template", "release", staged]
    for k, v in LINT_VALUES.items():
        cmd += ["--set", f"{k}={v}"]
    out = subprocess.run(cmd, capture_output=True, text=True)
    shutil.rmtree(staged, ignore_errors=True)
    if out.returncode != 0:
        return None, out.stderr.strip()
    return out.stdout, None


def check(app, docs, fail):
    kinds = {}
    for d in docs:
        kinds.setdefault(d["kind"], []).append(d)

    # Game servers expose raw TCP/UDP through WireGuard and run no Caddy at all,
    # so "has a Caddyfile" is a property of the shape, not a requirement.
    has_caddy = any(
        "Caddyfile" in (c.get("data") or {}) for c in kinds.get("ConfigMap", [])
    )

    for kind, want in (("PersistentVolumeClaim", 1), ("Job", 1)):
        got = len(kinds.get(kind, []))
        if got != want:
            fail(app, f"expected {want} {kind}, got {got}")

    # The uninstall hook must run before the release's resources are torn down;
    # as a normal manifest it would be deleted along with everything else and the
    # tunnel would leak.
    job = (kinds.get("Job") or [{}])[0]
    if job.get("metadata", {}).get("annotations", {}).get("helm.sh/hook") != "pre-delete":
        fail(app, "uninstall Job is not a pre-delete hook")

    deploys = {d["metadata"]["name"]: d for d in kinds.get("Deployment", [])}

    # The tunnel pod is wherever wg-register runs — usually a Deployment named
    # "gateway", but some charts fold the gateway containers into their own.
    tunnel_pods = [
        (n, d["spec"]["template"]["spec"])
        for n, d in deploys.items()
        if any(
            c["name"] == "wg-register"
            for c in d["spec"]["template"]["spec"].get("initContainers") or []
        )
    ]
    if len(tunnel_pods) != 1:
        fail(app, f"expected exactly one pod running wg-register, found {len(tunnel_pods)}")
        return
    gw_name, gw = tunnel_pods[0]
    conts = {c["name"]: c for c in gw.get("containers") or []}

    for req in GATEWAY_CONTAINERS if has_caddy else ("wireguard",):
        if req not in conts:
            fail(app, f"pod {gw_name} missing {req} container")

    if conts.get("wireguard", {}).get("securityContext", {}).get("privileged") is not True:
        fail(app, "wireguard sidecar is not privileged (the tunnel cannot come up)")

    for name, c in conts.items():
        if name != "wireguard" and c.get("securityContext", {}).get("privileged"):
            fail(app, f"container {name} is privileged but is not the tunnel sidecar")

    for dname, d in deploys.items():
        spec = d["spec"]["template"]["spec"]

        # An app that needs its own public URL learns it by sourcing /yolab/env,
        # which something has to write first: wg-register in the gateway pod, or
        # yolab-env in a pod of its own. Reference the variable without both the
        # mount and a writer and the app starts with it empty — which for these
        # apps means a permanent install record built around a blank hostname,
        # not a crash. Nothing else in the rendered YAML would show it.
        inits = {c["name"]: c for c in spec.get("initContainers") or []}
        writes_yolab_env = any(n in inits for n in ("wg-register", "yolab-env"))
        for c in spec.get("containers") or []:
            uses = "YOLAB_FQDN" in json.dumps(c) or "YOLAB_URL" in json.dumps(c)
            if not uses:
                continue
            if not any(m["mountPath"] == "/yolab" for m in c.get("volumeMounts") or []):
                fail(app, f"pod {dname}: container {c['name']} reads YOLAB_* but does "
                          f"not mount /yolab")
            if not writes_yolab_env:
                fail(app, f"pod {dname}: container {c['name']} reads YOLAB_* but no init "
                          f"container writes /yolab/env (needs wg-register or yolab-env)")

        # Several containers start with a `sh -c` script that sources /yolab/env,
        # exports the app's own-URL variables, then execs the image's entrypoint.
        # Those scripts carry nested quoting (Linkwarden's reproduces a CMD that
        # itself contains an `sh -c "…"`), and a quoting mistake renders as
        # perfectly valid YAML and crashloops the container. Parse them.
        for c in (spec.get("containers") or []) + (spec.get("initContainers") or []):
            cmd = c.get("command") or []
            if len(cmd) >= 3 and cmd[0] in ("/bin/sh", "sh", "/bin/bash") and cmd[1] == "-c":
                syntax = subprocess.run(
                    ["sh", "-n"], input=cmd[2], capture_output=True, text=True
                )
                if syntax.returncode != 0:
                    fail(app, f"pod {dname}: container {c['name']} command is not valid "
                              f"shell: {syntax.stderr.strip()}")

        # Containers in a pod share one network namespace, so two claiming the
        # same port means whichever starts second fails to bind — silently.
        seen = {}
        for c in spec.get("containers") or []:
            for p in c.get("ports") or []:
                cp = p["containerPort"]
                if cp in seen:
                    fail(app, f"pod {dname}: containerPort {cp} claimed by both "
                              f"{seen[cp]} and {c['name']}")
                seen[cp] = c["name"]

        vols = {v["name"] for v in spec.get("volumes") or []}
        for c in (spec.get("containers") or []) + (spec.get("initContainers") or []):
            for m in c.get("volumeMounts") or []:
                if m["name"] not in vols:
                    fail(app, f"pod {dname}: container {c['name']} mounts undeclared "
                              f"volume {m['name']}")

    # Every Caddy upstream must resolve to this pod or to a Service that exists.
    if has_caddy:
        caddyfile = next(
            c["data"]["Caddyfile"] for c in kinds["ConfigMap"] if "Caddyfile" in (c.get("data") or {})
        )
        ups = re.findall(r"reverse_proxy\s+(\S+)", caddyfile)
        if not ups:
            fail(app, "Caddyfile has no reverse_proxy directive")
        svcs = {s["metadata"]["name"]: s for s in kinds.get("Service", [])}
        for up in ups:
            # Helm does not re-render values, so a `{{ … }}` left in a value is
            # emitted literally and Caddy proxies to a host that cannot resolve.
            if "{{" in up or "}}" in up:
                fail(app, f"upstream {up!r} still contains an unrendered template expression")
                continue
            host, _, port = up.rpartition(":")
            if host == "localhost":
                if not [n for n in conts if n not in GATEWAY_CONTAINERS]:
                    fail(app, f"upstream {up} is localhost but no app container runs in {gw_name}")
                if port in ("80", "443"):
                    fail(app, f"upstream port {port} collides with Caddy in the same pod")
            elif host not in svcs:
                fail(app, f"upstream {up} names Service '{host}' which the chart does not create")
            else:
                exposed = {str(p["port"]) for p in svcs[host]["spec"]["ports"]}
                if port not in exposed:
                    fail(app, f"upstream {up}: Service {host} exposes {sorted(exposed)}, not {port}")

    pod_labels = [
        tuple(sorted(d["spec"]["template"]["metadata"]["labels"].items()))
        for d in deploys.values()
    ]
    for s in kinds.get("Service", []):
        sel = tuple(sorted(s["spec"]["selector"].items()))
        if not any(all(kv in lbl for kv in sel) for lbl in pod_labels):
            fail(app, f"Service {s['metadata']['name']} selector {dict(sel)} matches no pod")

    for d in list(deploys.values()) + kinds.get("Job", []):
        spec = d["spec"]["template"]["spec"]
        for c in (spec.get("containers") or []) + (spec.get("initContainers") or []):
            # An unpinned tag means two nodes can run different code from the same
            # release, and a restore can never reproduce what wrote the data.
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

    # An empty credential is worse than a missing one: the app starts, and the
    # blank password is accepted.
    for s in kinds.get("Secret", []):
        for k, v in (s.get("stringData") or {}).items():
            if v == "":
                fail(app, f"Secret key {k} rendered empty")


def main(argv):
    chart_dirs = argv[1:] or sorted(
        d for d in glob.glob(os.path.join(HERE, "*/"))
        if os.path.isfile(os.path.join(d, "Chart.yaml"))
        and "type: library" not in open(os.path.join(d, "Chart.yaml")).read()
    )
    if not chart_dirs:
        print("no charts found", file=sys.stderr)
        return 1

    lib_version = chart_field(open(os.path.join(LIBRARY, "Chart.yaml")).read(), "version")
    fail = Failures()

    with tempfile.TemporaryDirectory() as tmp:
        subprocess.run(
            ["helm", "package", LIBRARY, "--version", lib_version, "--destination", tmp],
            check=True, capture_output=True,
        )
        library_tgz = glob.glob(os.path.join(tmp, "yolab-common-*.tgz"))[0]

        for chart_dir in chart_dirs:
            app = os.path.basename(chart_dir.rstrip("/"))
            text = open(os.path.join(chart_dir, "Chart.yaml")).read()

            # A chart pinned to a library version other than the one in this tree
            # is rendered against something that is not what would ship with it.
            declared = re.search(r"- name: yolab-common\s*\n\s*version:\s*\"?([^\"\n]+)", text)
            if declared and declared.group(1).strip() != lib_version:
                fail(app, f"depends on yolab-common {declared.group(1).strip()}, "
                          f"but this tree has {lib_version}")

            rendered, err = render(chart_dir, library_tgz, tmp)
            if rendered is None:
                fail(app, f"helm template failed: {err.splitlines()[-1] if err else 'unknown'}")
                continue
            try:
                docs = [d for d in yaml.safe_load_all(rendered) if d]
            except yaml.YAMLError as e:
                fail(app, f"rendered invalid YAML: {e}")
                continue
            check(app, docs, fail)

    print(f"checked {len(chart_dirs)} charts")
    for f in fail.items:
        print("FAIL " + f)
    print(f"FAILURES: {len(fail)}" if len(fail) else "all assertions passed")
    return 1 if len(fail) else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
