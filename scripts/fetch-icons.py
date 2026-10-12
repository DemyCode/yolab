#!/usr/bin/env python3
"""Download a real logo for every catalog app and regenerate the manifest.

    nix develop --command python3 scripts/fetch-icons.py

Run this after adding a chart. Icons are committed and served from the box
rather than hotlinked: a house with no working internet is exactly when someone
opens this dashboard, and one CDN request per app would also tell a third party
which apps each household runs.

Sources, in order: selfh.st/icons, homarr-labs/dashboard-icons, then the Umbrel
app gallery. Each is matched by the app's id, its display name and its GitHub
repository name. Only full-colour variants are taken. An SVG too large to ship
(Umbrel's usually wrap a big raster) is rendered to a PNG with rsvg-convert,
and a result with no visible content is rejected. Packs keep their emoji.
"""

import json
import os
import re
import subprocess
import sys
import tempfile

import yaml

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
CATALOG = os.path.join(ROOT, "apps/catalog")
OUT_DIR = os.path.join(ROOT, "homelab/client-ui/public/icons")
MANIFEST = os.path.join(ROOT, "homelab/client-ui/src/catalog/icons.ts")

SELFHST = ("selfhst/icons", "main", "https://cdn.jsdelivr.net/gh/selfhst/icons")
DASHBOARD = (
    "homarr-labs/dashboard-icons",
    "main",
    "https://cdn.jsdelivr.net/gh/homarr-labs/dashboard-icons",
)
UMBREL = (
    "getumbrel/umbrel-apps-gallery",
    "master",
    "https://getumbrel.github.io/umbrel-apps-gallery",
)

LIBRARIES = {"yolab-common", "yolab-stack"}

ALIASES = {
    "actual": "actual-budget",
    "code-server": "vscode",
}

GITHUB_AVATAR = {
    "stash": "savewithstash",
}

NOT_THIS_PROJECT = {
    "stash",
}

SVG_MAX_BYTES = 40_000
PNG_MAX_PX = 192


def fetch(url: str) -> bytes | None:
    r = subprocess.run(
        ["curl", "-sfL", "--max-time", "30", url], capture_output=True, check=False
    )
    return r.stdout if r.returncode == 0 and r.stdout else None


def tree(repo: str, branch: str) -> set[str]:
    raw = fetch(f"https://api.github.com/repos/{repo}/git/trees/{branch}?recursive=1")
    if raw is None:
        sys.exit(f"could not list {repo}")
    return {entry["path"] for entry in json.loads(raw)["tree"]}


def slug(text: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", text.lower()).strip("-")


def candidates(app_id: str, chart: dict) -> list[str]:
    notes = chart.get("annotations") or {}
    name = slug(str(notes.get("yolab.io/display-name") or ""))
    repo = slug(str(notes.get("yolab.io/github") or "").split("/")[-1])
    found = [
        ALIASES.get(app_id, app_id),
        app_id,
        name,
        repo,
        re.sub(r"-(ngx|server|web)$", "", app_id),
        name.replace("-", ""),
    ]
    return [c for i, c in enumerate(found) if c and c not in found[:i]]


def is_pack(chart: dict) -> bool:
    deps = chart.get("dependencies") or []
    return any(d.get("name") not in LIBRARIES for d in deps if isinstance(d, dict))


def visible(path: str) -> bool:
    r = subprocess.run(
        ["magick", path, "-format", "%[fx:standard_deviation]", "info:"],
        capture_output=True,
        text=True,
        check=False,
    )
    try:
        return float(r.stdout.strip()) > 0.0
    except ValueError:
        return False


def as_png(data: bytes, svg: bool, out: str) -> bool:
    with tempfile.NamedTemporaryFile(suffix=".svg" if svg else ".img") as src:
        src.write(data)
        src.flush()
        if svg:
            cmd = ["rsvg-convert", "-w", str(PNG_MAX_PX), "-h", str(PNG_MAX_PX)]
            cmd += ["-a", src.name, "-o", out]
        else:
            size = f"{PNG_MAX_PX}x{PNG_MAX_PX}"
            cmd = ["magick", src.name, "-resize", size, "-depth", "8", "-strip", out]
        ok = subprocess.run(cmd, capture_output=True, check=False).returncode == 0
    return ok and os.path.exists(out) and visible(out)


def offers(app_id: str, slugs: list[str], trees: dict) -> list[tuple[str, bool]]:
    found = []
    if app_id not in NOT_THIS_PROJECT:
        for source in (SELFHST, DASHBOARD):
            for s in slugs:
                if f"svg/{s}.svg" in trees[source]:
                    found.append((f"{source[2]}/svg/{s}.svg", True))
        for s in slugs:
            if f"{s}/icon.svg" in trees[UMBREL]:
                found.append((f"{UMBREL[2]}/{s}/icon.svg", True))
        for source in (SELFHST, DASHBOARD):
            for s in slugs:
                if f"png/{s}.png" in trees[source]:
                    found.append((f"{source[2]}/png/{s}.png", False))
    if app_id in GITHUB_AVATAR:
        found.append((f"https://github.com/{GITHUB_AVATAR[app_id]}.png", False))
    return found


def store(app_id: str, url: str, svg: bool) -> str | None:
    data = fetch(url)
    if data is None:
        return None
    if svg and len(data) <= SVG_MAX_BYTES and b"<svg" in data[:2000]:
        with open(os.path.join(OUT_DIR, f"{app_id}.svg"), "wb") as f:
            f.write(data)
        return f"/icons/{app_id}.svg"
    out = os.path.join(OUT_DIR, f"{app_id}.png")
    if as_png(data, svg, out):
        return f"/icons/{app_id}.png"
    if os.path.exists(out):
        os.remove(out)
    return None


def key(app_id: str) -> str:
    return app_id if re.fullmatch(r"[a-z_][a-z0-9_]*", app_id) else f'"{app_id}"'


def main() -> int:
    trees = {s: tree(s[0], s[1]) for s in (SELFHST, DASHBOARD, UMBREL)}
    charts = {}
    for d in sorted(os.listdir(CATALOG)):
        path = os.path.join(CATALOG, d, "Chart.yaml")
        if not os.path.isfile(path):
            continue
        with open(path) as f:
            chart = yaml.safe_load(f) or {}
        if chart.get("type") == "library" or is_pack(chart):
            continue
        charts[d] = chart

    os.makedirs(OUT_DIR, exist_ok=True)
    for old in os.listdir(OUT_DIR):
        os.remove(os.path.join(OUT_DIR, old))

    found: dict[str, str] = {}
    missing: list[str] = []
    for app_id, chart in charts.items():
        for url, svg in offers(app_id, candidates(app_id, chart), trees):
            stored = store(app_id, url, svg)
            if stored:
                found[app_id] = stored
                break
        else:
            missing.append(app_id)

    lines = ["export const CATALOG_ICONS: Record<string, string> = {"]
    lines += [f'  {key(k)}: "{found[k]}",' for k in sorted(found)]
    lines += ["};", ""]
    with open(MANIFEST, "w") as f:
        f.write("\n".join(lines))

    print(f"icons: {len(found)} of {len(charts)}")
    if missing:
        print("no logo found (emoji fallback):", " ".join(missing))
    return 0


if __name__ == "__main__":
    sys.exit(main())
