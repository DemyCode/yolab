#!/usr/bin/env python3
"""Update every pinned dependency of yolab and yolab-external in one go.

    nix run .#update                 # this repo and ../yolab-external
    nix run .#update -- --only images,actions
    nix run .#update -- /path/to/repo [/path/to/other-repo]

Nothing is committed: review `git diff`, run the checks, commit. Every step
leaves its files exactly as they were when it fails, and the summary at the end
lists what moved, what failed and what was held back on purpose.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor

import yaml

OWN_IMAGES = "ghcr.io/demycode/"
SKIP_DIRS = {".git", "node_modules", "target", "dist", "result", ".terraform"}
CACHE = "https://cache.nixos.org"
STEPS = ("flake", "cargo", "npm", "images", "helm", "actions", "tofu")

PINNED = re.compile(
    r"(?<![\w/.\-])([a-z0-9.\-]+(?::[0-9]+)?(?:/[a-zA-Z0-9._\-]+)*)"
    r"(?::([A-Za-z0-9._\-]+))?@(sha256:[0-9a-f]{64})"
)
IMAGE_LINE = re.compile(
    r"^(\s*-?\s*image:\s*[\"']?)([^\s\"'{}@]+:[^\s\"'{}@]+)([\"']?\s*)$"
)
FROM_LINE = re.compile(r"^(FROM\s+(?:--platform=\S+\s+)?)(\S+)(.*)$")
USES_LINE = re.compile(
    r"^(\s*-?\s*uses:\s*)([A-Za-z0-9_.\-]+/[A-Za-z0-9_.\-]+)((?:/[^@\s]+)?)@(\S+)(\s*#.*)?$"
)
TAG_SHAPE = re.compile(r"^(v?)(\d+(?:\.\d+)*)([^.\d].*)?$")
SEMVER = re.compile(r"^v?(\d+)\.(\d+)\.(\d+)$")


class Summary:
    def __init__(self):
        self.moved = []
        self.failed = []
        self.held = []

    def report(self):
        for title, rows in (
            ("Updated", self.moved),
            ("Held back on purpose", self.held),
            ("Failed (files left as they were)", self.failed),
        ):
            print(f"\n== {title}: {len(rows)}")
            for row in rows:
                print(f"  {row}")
        return 1 if self.failed else 0


def log(msg):
    print(msg, file=sys.stderr, flush=True)


def run(cmd, cwd=None, timeout=1800, capture=True):
    log(f"$ {' '.join(cmd)}" + (f"   (in {cwd})" if cwd else ""))
    out = subprocess.run(
        cmd,
        cwd=cwd,
        capture_output=capture,
        timeout=timeout,
        check=False,
    )
    if out.returncode != 0:
        tail = (out.stderr or b"").decode(errors="replace").strip().splitlines()[-3:]
        raise RuntimeError(f"{' '.join(cmd[:3])} failed: {' / '.join(tail)}")
    return out.stdout or b""


def walk(root, keep):
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = [d for d in dirnames if d not in SKIP_DIRS]
        for name in filenames:
            path = os.path.join(dirpath, name)
            if keep(path):
                yield path


def read(path):
    with open(path) as f:
        return f.read()


def write(path, text):
    with open(path, "w") as f:
        f.write(text)


class Snapshot:
    def __init__(self, paths):
        self.saved = {p: read(p) if os.path.exists(p) else None for p in paths}

    def restore(self):
        for path, text in self.saved.items():
            if text is None:
                if os.path.exists(path):
                    os.remove(path)
            else:
                write(path, text)


def step(summary, name, paths, fn):
    snap = Snapshot(paths)
    try:
        fn()
    except Exception as e:  # noqa: BLE001
        snap.restore()
        summary.failed.append(f"{name}: {e}")


def tag_shape(tag):
    m = TAG_SHAPE.match(tag or "")
    if not m:
        return None
    return m.group(1), tuple(int(n) for n in m.group(2).split(".")), m.group(3) or ""


def same_line(nums, other):
    if len(nums) != len(other):
        return False
    if nums[0] >= 1900:
        return True
    if nums[0] == 0:
        return other[:2] == nums[:2] if len(nums) > 1 else other == nums
    return other[0] == nums[0]


def newer_tag(tag, candidates):
    shape = tag_shape(tag)
    if shape is None:
        return None
    prefix, nums, suffix = shape
    best = None
    for c in candidates:
        cs = tag_shape(c)
        if cs is None or cs[0] != prefix or cs[2] != suffix:
            continue
        if not same_line(nums, cs[1]) or cs[1] <= nums:
            continue
        if best is None or cs[1] > tag_shape(best)[1]:
            best = c
    return best


def newest_major(tag, candidates):
    shape = tag_shape(tag)
    if shape is None:
        return None
    prefix, nums, suffix = shape
    best = None
    for c in candidates:
        cs = tag_shape(c)
        if cs is None or cs[0] != prefix or cs[2] != suffix or len(cs[1]) != len(nums):
            continue
        if cs[1] > nums and not same_line(nums, cs[1]):
            if best is None or cs[1] > tag_shape(best)[1]:
                best = c
    return best


def newest_patch(version, candidates):
    m = SEMVER.match(version)
    if not m:
        return None
    cur = tuple(int(x) for x in m.groups())
    best = None
    for c in candidates:
        cm = SEMVER.match(c)
        if not cm:
            continue
        cv = tuple(int(x) for x in cm.groups())
        if cv[:2] == cur[:2] and cv > cur and (best is None or cv > best[0]):
            best = (cv, c)
    return best[1] if best else None


def newest_release(tags):
    best = None
    for t in tags:
        m = SEMVER.match(t)
        if m:
            v = tuple(int(x) for x in m.groups())
            if best is None or v > best[0]:
                best = (v, t)
    return best[1] if best else None


def bump_patch(version):
    major, minor, patch = str(version).split(".")
    return f"{major}.{minor}.{int(patch) + 1}"


def split_ref(ref):
    digest = None
    if "@" in ref:
        ref, digest = ref.split("@", 1)
    slash = ref.rfind("/")
    colon = ref.rfind(":")
    if colon > slash:
        return ref[:colon], ref[colon + 1 :], digest
    return ref, None, digest


def join_ref(name, tag, digest):
    return f"{name}{':' + tag if tag else ''}@{digest}"


def image_files(root):
    def keep(path):
        base = os.path.basename(path)
        if "/.github/" in path or base in ("image_arches.json", "catalog.yaml"):
            return False
        return base.startswith("Dockerfile") or base.endswith((".yaml", ".yml", ".tpl"))

    return list(walk(root, keep))


def image_refs(text):
    refs = {m.group(0) for m in PINNED.finditer(text)}
    for line in text.splitlines():
        m = IMAGE_LINE.match(line)
        if m:
            refs.add(m.group(2))
            continue
        m = FROM_LINE.match(line)
        if m and "@" not in m.group(2) and ":" in m.group(2) and "$" not in m.group(2):
            refs.add(m.group(2))
    return {r for r in refs if not r.startswith(OWN_IMAGES)}


def skopeo_tags(name):
    out = run(["skopeo", "list-tags", f"docker://{name}"], timeout=180)
    return json.loads(out).get("Tags") or []


def skopeo_digest(name, tag):
    raw = run(["skopeo", "inspect", "--raw", f"docker://{name}:{tag}"], timeout=180)
    return "sha256:" + hashlib.sha256(raw).hexdigest()


def resolve_image(ref):
    name, tag, digest = split_ref(ref)
    if tag is None:
        return ref, None, f"{ref}: no tag to follow, left as is"
    tags = skopeo_tags(name)
    target = newer_tag(tag, tags) or tag
    major = newest_major(target, tags)
    new = join_ref(name, target, skopeo_digest(name, target))
    note = (
        f"{name}: {major} exists, a new major line, left for a person"
        if major
        else None
    )
    return ref, (new if new != ref else None), note


def update_images(root, summary):
    files = image_files(root)
    texts = {p: read(p) for p in files}
    refs = sorted({r for t in texts.values() for r in image_refs(t)})
    log(f"{len(refs)} image references in {root}")

    def safe(ref):
        try:
            return resolve_image(ref)
        except Exception as e:  # noqa: BLE001
            return ref, None, f"FAILED {ref}: {e}"

    with ThreadPoolExecutor(max_workers=8) as pool:
        results = list(pool.map(safe, refs))

    changes = {}
    for old, new, note in results:
        if note and note.startswith("FAILED"):
            summary.failed.append(f"image {note[7:]}")
        elif note:
            summary.held.append(f"image {note}")
        if new:
            changes[old] = new
            summary.moved.append(f"image {old.split('@')[0]} -> {new.split('@')[0]}")

    touched = []
    for path, text in texts.items():
        new_text = replace_refs(text, changes)
        if new_text != text:
            write(path, new_text)
            touched.append(path)
    return touched


def replace_refs(text, changes):
    if not changes:
        return text
    pattern = re.compile(
        "|".join(
            r"(?<![\w/.\-])" + re.escape(old) + r"(?![\w.\-@:])"
            for old in sorted(changes, key=len, reverse=True)
        )
    )
    return pattern.sub(lambda m: changes[m.group(0)], text)


TOP_VERSION = re.compile(r"^version:\s*[\"']?([0-9]+\.[0-9]+\.[0-9]+)[\"']?\s*$", re.M)
LIB_DEP = re.compile(
    r"(- name: yolab-common\n\s+version: )[\"']?[0-9]+\.[0-9]+\.[0-9]+[\"']?"
)


def chart_of(catalog, path):
    rel = os.path.relpath(path, catalog)
    head = rel.split(os.sep, 1)[0]
    return head if os.path.isfile(os.path.join(catalog, head, "Chart.yaml")) else None


def head_version(root, chart_yaml):
    rel = os.path.relpath(chart_yaml, root)
    try:
        text = run(["git", "show", f"HEAD:{rel}"], cwd=root).decode()
    except RuntimeError:
        return None
    m = TOP_VERSION.search(text)
    return m.group(1) if m else None


def bump_chart(root, chart_yaml, summary):
    text = read(chart_yaml)
    m = TOP_VERSION.search(text)
    if not m:
        summary.failed.append(f"chart {chart_yaml}: no version line")
        return text, None
    if head_version(root, chart_yaml) not in (None, m.group(1)):
        return text, m.group(1)
    new = bump_patch(m.group(1))
    text = text[: m.start(1)] + new + text[m.end(1) :]
    write(chart_yaml, text)
    return text, new


def bump_charts(root, touched, summary):
    catalog = os.path.join(root, "apps", "catalog")
    if not os.path.isdir(catalog):
        return
    charts = {
        c for c in (chart_of(catalog, p) for p in touched if p.startswith(catalog)) if c
    }
    if not charts:
        return
    if "yolab-common" in charts:
        _, lib = bump_chart(
            root, os.path.join(catalog, "yolab-common", "Chart.yaml"), summary
        )
        charts.discard("yolab-common")
        for name in sorted(os.listdir(catalog)):
            chart_yaml = os.path.join(catalog, name, "Chart.yaml")
            if name == "yolab-common" or not os.path.isfile(chart_yaml):
                continue
            text = read(chart_yaml)
            new = LIB_DEP.sub(lambda m: f'{m.group(1)}"{lib}"', text)
            if new != text:
                write(chart_yaml, new)
                charts.add(name)
        importer = os.path.join(catalog, "import_umbrel.py")
        if os.path.isfile(importer):
            write(
                importer,
                re.sub(
                    r'^LIB_VERSION = "[^"]+"',
                    f'LIB_VERSION = "{lib}"',
                    read(importer),
                    flags=re.M,
                ),
            )
        summary.moved.append(f"chart yolab-common -> {lib} (every app chart follows)")
    for name in sorted(charts):
        _, v = bump_chart(root, os.path.join(catalog, name, "Chart.yaml"), summary)
        if v:
            summary.moved.append(f"chart {name} -> {v}")
    arches = os.path.join(catalog, "image_arches.py")
    if os.path.isfile(arches):
        step(
            summary,
            "image_arches.json",
            [os.path.join(catalog, "image_arches.json")],
            lambda: run(
                [sys.executable, arches], cwd=root, capture=False, timeout=3600
            ),
        )


def helm_index(repo):
    out = run(["curl", "-fsSL", repo.rstrip("/") + "/index.yaml"], timeout=180)
    return yaml.safe_load(out) or {}


def update_helmcharts(root, summary):
    touched = []
    for path in walk(
        root, lambda p: p.endswith((".yaml", ".yml")) and "/apps/catalog/" not in p
    ):
        text = read(path)
        if "kind: HelmChart" not in text:
            continue
        new_text = text
        for doc in yaml.safe_load_all(text):
            if not doc or doc.get("kind") != "HelmChart":
                continue
            spec = doc.get("spec") or {}
            repo, chart, version = (
                spec.get("repo"),
                spec.get("chart"),
                str(spec.get("version") or ""),
            )
            if not (repo and chart and version):
                continue
            try:
                entries = (helm_index(repo).get("entries") or {}).get(chart) or []
            except Exception as e:  # noqa: BLE001
                summary.failed.append(f"helm {chart}: {e}")
                continue
            versions = [str(e.get("version")) for e in entries]
            target = newest_patch(version, versions)
            newest = newest_release(versions)
            if newest and newest != (target or version):
                summary.held.append(
                    f"helm {chart}: {newest} exists; upgrade one minor at a time by hand"
                )
            if target:
                new_text = re.sub(
                    r"(\n\s+version:\s*)" + re.escape(version) + r"\b",
                    lambda m: m.group(1) + target,
                    new_text,
                    count=1,
                )
                summary.moved.append(f"helm {chart} {version} -> {target}")
        if new_text != text:
            write(path, new_text)
            touched.append(path)
    return touched


def parse_ls_remote(text):
    out = {}
    for line in text.splitlines():
        sha, _, ref = line.partition("\t")
        if not ref.startswith("refs/tags/"):
            continue
        tag = ref[len("refs/tags/") :]
        if tag.endswith("^{}"):
            out[tag[:-3]] = sha
        else:
            out.setdefault(tag, sha)
    return out


def pin_uses(line, resolve):
    m = USES_LINE.match(line)
    if not m:
        return line
    lead, repo, sub, _, _ = m.groups()
    found = resolve(repo)
    if not found:
        return line
    tag, sha = found
    return f"{lead}{repo}{sub}@{sha} # {tag}"


def update_actions(root, summary):
    files = list(
        walk(os.path.join(root, ".github"), lambda p: p.endswith((".yml", ".yaml")))
    )
    cache = {}

    def resolve(repo):
        if repo not in cache:
            try:
                text = run(
                    ["git", "ls-remote", "--tags", f"https://github.com/{repo}"]
                ).decode()
                tags = parse_ls_remote(text)
                tag = newest_release(tags)
                cache[repo] = (tag, tags[tag]) if tag else None
            except Exception as e:  # noqa: BLE001
                summary.failed.append(f"action {repo}: {e}")
                cache[repo] = None
        return cache[repo]

    touched = []
    for path in files:
        text = read(path)
        new_text = "\n".join(pin_uses(line, resolve) for line in text.split("\n"))
        if new_text != text:
            write(path, new_text)
            touched.append(path)
    for repo, found in sorted(cache.items()):
        if found:
            summary.moved.append(f"action {repo} -> {found[0]}")
    return touched


def flake_inputs(lock):
    nodes = lock["nodes"]
    root = nodes[lock["root"]]["inputs"]
    names = []
    for name, ref in root.items():
        node = nodes.get(ref if isinstance(ref, str) else "", {})
        if (node.get("locked") or {}).get("type") not in (None, "path"):
            names.append(name)
    return sorted(names)


def nixpkgs_attr(rev, attr):
    return run(
        [
            "nix",
            "eval",
            "--raw",
            f"github:NixOS/nixpkgs/{rev}#legacyPackages.x86_64-linux.{attr}",
        ],
        timeout=900,
    ).decode()


def is_cached(out_path):
    try:
        run(["nix", "path-info", "--store", CACHE, out_path], timeout=120)
        return True
    except RuntimeError:
        return False


def minor_of(version):
    m = re.match(r"(\d+)\.(\d+)", version)
    return (int(m.group(1)), int(m.group(2))) if m else None


def nixpkgs_problem(old_rev, new_rev, needs_ceph):
    old_k3s = minor_of(nixpkgs_attr(old_rev, "k3s.version"))
    new_k3s = minor_of(nixpkgs_attr(new_rev, "k3s.version"))
    if (
        old_k3s
        and new_k3s
        and (new_k3s[0] != old_k3s[0] or new_k3s[1] > old_k3s[1] + 1)
    ):
        return f"k3s would jump {old_k3s[0]}.{old_k3s[1]} -> {new_k3s[0]}.{new_k3s[1]}"
    if needs_ceph and not is_cached(nixpkgs_attr(new_rev, "ceph.outPath")):
        return "ceph is not in cache.nixos.org (it failed to build upstream)"
    return None


def update_flake(root, summary):
    path = os.path.join(root, "flake.lock")
    if not os.path.isfile(path):
        return
    before = json.loads(read(path))
    old_rev = (before["nodes"].get("nixpkgs", {}).get("locked") or {}).get("rev")
    names = flake_inputs(before)
    if not names:
        return
    run(["nix", "flake", "update", *names], cwd=root, capture=False)
    after = json.loads(read(path))
    new_rev = (after["nodes"].get("nixpkgs", {}).get("locked") or {}).get("rev")
    if old_rev and new_rev and old_rev != new_rev:
        problem = nixpkgs_problem(
            old_rev,
            new_rev,
            os.path.isdir(os.path.join(root, "homelab", "nixos", "ceph")),
        )
        if problem:
            after["nodes"]["nixpkgs"] = before["nodes"]["nixpkgs"]
            write(path, json.dumps(after, indent=2) + "\n")
            summary.held.append(
                f"nixpkgs in {os.path.basename(root)} stays at {old_rev[:7]}: {problem}"
            )
    for name, node in after["nodes"].items():
        old = (before["nodes"].get(name, {}).get("locked") or {}).get("rev")
        new = (node.get("locked") or {}).get("rev")
        if old and new and old != new:
            summary.moved.append(
                f"flake {os.path.basename(root)}/{name} {old[:7]} -> {new[:7]}"
            )


def update_cargo(root, summary):
    for lock in walk(root, lambda p: os.path.basename(p) == "Cargo.lock"):
        crate = os.path.dirname(lock)
        manifests = list(walk(crate, lambda p: os.path.basename(p) == "Cargo.toml"))

        rel = os.path.relpath(crate, root)

        def go(crate=crate, rel=rel):
            run(
                ["cargo", "upgrade", "--incompatible", "allow", "--pinned", "allow"],
                cwd=crate,
                capture=False,
            )
            run(["cargo", "update"], cwd=crate, capture=False)
            summary.moved.append(f"cargo {rel}")

        step(summary, f"cargo {rel}", [lock, *manifests], go)


def npm_deps_hash(lock):
    return run(["prefetch-npm-deps", lock], timeout=1800).decode().strip()


def replace_in_nix(root, old, new):
    hits = []
    for path in walk(root, lambda p: p.endswith(".nix")):
        text = read(path)
        if old in text:
            write(path, text.replace(old, new))
            hits.append(path)
    return hits


def update_npm(root, summary):
    for lock in walk(root, lambda p: os.path.basename(p) == "package-lock.json"):
        pkg_dir = os.path.dirname(lock)
        rel = os.path.relpath(pkg_dir, root)
        nix_files = list(walk(root, lambda p: p.endswith(".nix")))

        def go(pkg_dir=pkg_dir, lock=lock, rel=rel):
            old_hash = npm_deps_hash(lock)
            run(["ncu", "--upgrade"], cwd=pkg_dir, capture=False)
            run(
                [
                    "npm",
                    "install",
                    "--package-lock-only",
                    "--ignore-scripts",
                    "--no-audit",
                    "--no-fund",
                ],
                cwd=pkg_dir,
                capture=False,
            )
            new_hash = npm_deps_hash(lock)
            if new_hash != old_hash and not replace_in_nix(root, old_hash, new_hash):
                raise RuntimeError(
                    f"no .nix file holds npmDepsHash {old_hash} for {rel}"
                )
            summary.moved.append(f"npm {rel}")

        step(
            summary,
            f"npm {rel}",
            [lock, os.path.join(pkg_dir, "package.json"), *nix_files],
            go,
        )


def update_tofu(root, summary):
    for lock in walk(root, lambda p: os.path.basename(p) == ".terraform.lock.hcl"):
        tf_dir = os.path.dirname(lock)
        rel = os.path.relpath(tf_dir, root)

        def go(tf_dir=tf_dir, rel=rel):
            try:
                run(
                    ["tofu", "init", "-upgrade", "-backend=false", "-input=false"],
                    cwd=tf_dir,
                    capture=False,
                )
            finally:
                shutil.rmtree(os.path.join(tf_dir, ".terraform"), ignore_errors=True)
            summary.moved.append(f"tofu {rel}")

        step(summary, f"tofu {rel}", [lock], go)


def repos(args_repos):
    if args_repos:
        return [os.path.abspath(r) for r in args_repos]
    here = run(["git", "rev-parse", "--show-toplevel"]).decode().strip()
    sibling = os.path.join(os.path.dirname(here), "yolab-external")
    found = [here]
    if os.path.basename(here) != "yolab-external" and os.path.isdir(
        os.path.join(sibling, ".git")
    ):
        found.append(sibling)
    return found


def main(argv=None):
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("repos", nargs="*")
    ap.add_argument(
        "--only",
        default=",".join(STEPS),
        help=f"comma-separated subset of {','.join(STEPS)}",
    )
    args = ap.parse_args(argv)
    only = set(args.only.split(","))
    unknown = only - set(STEPS)
    if unknown:
        ap.error(f"unknown steps: {', '.join(sorted(unknown))}")

    summary = Summary()
    for root in repos(args.repos):
        log(f"\n##### {root}")
        if "flake" in only:
            step(
                summary,
                f"flake {os.path.basename(root)}",
                [os.path.join(root, "flake.lock")],
                lambda: update_flake(root, summary),
            )
        if "cargo" in only:
            update_cargo(root, summary)
        if "npm" in only:
            update_npm(root, summary)
        if "tofu" in only:
            update_tofu(root, summary)
        if "actions" in only:
            update_actions(root, summary)
        touched = []
        if "helm" in only:
            touched += update_helmcharts(root, summary)
        if "images" in only:
            touched += update_images(root, summary)
        bump_charts(root, touched, summary)
    return summary.report()


if __name__ == "__main__":
    sys.exit(main())
