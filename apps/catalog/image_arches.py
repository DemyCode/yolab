#!/usr/bin/env python3
"""Record which processors every pinned catalog image is published for.

check_charts.py reads image_arches.json offline and refuses a pod that could
land on an ARM machine while running an image built only for x86. This script
is the only thing that talks to the registries; run it whenever an image pin
changes:

    nix shell nixpkgs#skopeo -c python3 apps/catalog/image_arches.py
"""

import json
import os
import re
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "image_arches.json")
PINNED = re.compile(
    r"(?<![\w/.\-])[a-z0-9.\-]+(?:/[a-zA-Z0-9._\-]+)*(?::[A-Za-z0-9._\-]+)?@sha256:[0-9a-f]{64}"
)
OWN = "ghcr.io/demycode/"


def pinned_images():
    found = set()
    for root, _, files in os.walk(HERE):
        for name in files:
            if name.endswith((".yaml", ".tpl")):
                with open(os.path.join(root, name)) as f:
                    found.update(PINNED.findall(f.read()))
    return sorted(i for i in found if not i.startswith(OWN))


def without_tag(image):
    return re.sub(r":[^:@/]+@", "@", image)


def inspect(*args):
    out = subprocess.run(
        ["skopeo", "inspect", *args],
        capture_output=True,
        text=True,
        timeout=120,
        check=False,
    )
    if out.returncode != 0:
        raise RuntimeError(out.stderr.strip().splitlines()[-1])
    return json.loads(out.stdout)


def sources(image):
    bare = without_tag(image)
    name = bare.split("@", 1)[0]
    host = name.split("/", 1)[0]
    elsewhere = "/" in name and host != "docker.io" and ("." in host or ":" in host)
    if elsewhere:
        return [bare]
    path = bare.removeprefix("docker.io/")
    if "/" not in path.split("@", 1)[0]:
        path = f"library/{path}"
    return [f"mirror.gcr.io/{path}", bare]


def arches_at(ref):
    raw = inspect("--raw", ref)
    if "manifests" in raw:
        return {
            m.get("platform", {}).get("architecture")
            for m in raw["manifests"]
            if m.get("platform", {}).get("os") == "linux"
        }
    return {inspect("--config", ref).get("architecture")}


def arches(image):
    error = None
    for source in sources(image):
        try:
            found = arches_at(f"docker://{source}")
        except (RuntimeError, subprocess.TimeoutExpired) as e:
            error = e
            continue
        return sorted(a for a in found if a in ("amd64", "arm64"))
    print(f"unreadable {image}: {error}", file=sys.stderr)
    return None


def merged(pinned, previous, lookup):
    missing = [i for i in pinned if not previous.get(i)]
    with ThreadPoolExecutor(max_workers=8) as pool:
        fresh = dict(zip(missing, pool.map(lookup, missing)))
    result = {i: previous[i] for i in pinned if previous.get(i)}
    result.update({i: a for i, a in fresh.items() if a is not None})
    unreadable = sorted(i for i, a in fresh.items() if a is None)
    return result, unreadable


def main():
    try:
        with open(OUT) as f:
            previous = json.load(f)
    except FileNotFoundError:
        previous = {}
    result, unreadable = merged(pinned_images(), previous, arches)
    with open(OUT, "w") as f:
        json.dump(result, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"recorded {len(result)} images in {OUT}")
    if unreadable:
        print(
            f"{len(unreadable)} images could not be read and are left out; "
            "run this again later (or after `skopeo login docker.io`):",
            file=sys.stderr,
        )
        for image in unreadable:
            print(f"  {image}", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
