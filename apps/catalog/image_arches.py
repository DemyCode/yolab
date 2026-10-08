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


def arches(image):
    ref = f"docker://{without_tag(image)}"
    try:
        raw = inspect("--raw", ref)
        if "manifests" in raw:
            found = {
                m.get("platform", {}).get("architecture")
                for m in raw["manifests"]
                if m.get("platform", {}).get("os") == "linux"
            }
        else:
            found = {inspect("--config", ref).get("architecture")}
    except (RuntimeError, subprocess.TimeoutExpired) as e:
        print(f"unreadable {image}: {e}", file=sys.stderr)
        return None
    return sorted(a for a in found if a in ("amd64", "arm64"))


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
