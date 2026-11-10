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
TAG_SHAPE = re.compile(r"^((?:[A-Za-z][A-Za-z0-9_]*-)*v?)(\d+(?:\.\d+)*)([^.\d].*)?$")
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


def run(cmd, cwd=None, timeout=1800, capture=True, env=None, stderr=False):
    log(f"$ {' '.join(cmd)}" + (f"   (in {cwd})" if cwd else ""))
    out = subprocess.run(
        cmd,
        cwd=cwd,
        capture_output=capture,
        timeout=timeout,
        check=False,
        env={**os.environ, **env} if env else None,
    )
    if out.returncode != 0:
        tail = (out.stderr or b"").decode(errors="replace").strip().splitlines()[-3:]
        raise RuntimeError(f"{' '.join(cmd[:3])} failed: {' / '.join(tail)}")
    if stderr:
        return out.stdout or b"", out.stderr or b""
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
        newer_line = cs[1] > nums and not same_line(nums, cs[1])
        if newer_line and (best is None or cs[1] > tag_shape(best)[1]):
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


DISABLED = re.compile(r"^\s+yolab\.io/disabled:\s*\S", re.MULTILINE)


def disabled_chart_dirs(root):
    """Charts switched off with a yolab.io/disabled annotation. Users cannot
    install them, so their images are not followed (often they are disabled
    precisely because the image is gone)."""
    return [
        os.path.dirname(p) + os.sep
        for p in walk(root, lambda p: os.path.basename(p) == "Chart.yaml")
        if DISABLED.search(read(p))
    ]


def image_files(root):
    disabled = disabled_chart_dirs(root)

    def keep(path):
        base = os.path.basename(path)
        if "/.github/" in path or base in ("image_arches.json", "catalog.yaml"):
            return False
        if any(path.startswith(d) for d in disabled):
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


def registry_tags(name):
    return run(["crane", "ls", name], timeout=180).decode().split()


def registry_digest(name, tag):
    return run(["crane", "digest", f"{name}:{tag}"], timeout=180).decode().strip()


def resolve_image(ref):
    name, tag, _ = split_ref(ref)
    if tag is None:
        return ref, None, f"{ref}: no tag to follow, left as is"
    tags = registry_tags(name)
    target = newer_tag(tag, tags) or tag
    major = newest_major(target, tags)
    new = join_ref(name, target, registry_digest(name, target))
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


TOP_VERSION = re.compile(
    r"^version:\s*[\"']?([0-9]+\.[0-9]+\.[0-9]+)[\"']?\s*$", re.MULTILINE
)
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


DEP_PIN = re.compile(
    r"^(\s*-\s*name:\s*[\"']?([\w.-]+)[\"']?\s*\n\s+version:\s*)([\"']?)([^\"'\n]+)\3",
    re.MULTILINE,
)


def catalog_versions(catalog):
    found = {}
    for name in os.listdir(catalog):
        chart_yaml = os.path.join(catalog, name, "Chart.yaml")
        if os.path.isfile(chart_yaml):
            m = TOP_VERSION.search(read(chart_yaml))
            if m:
                found[name] = m.group(1)
    return found


def repin(text, versions):
    """Point every dependency on a chart of this catalog at the version the
    catalog has now."""
    return DEP_PIN.sub(
        lambda m: (
            f"{m.group(1)}{m.group(3)}{versions[m.group(2)]}{m.group(3)}"
            if m.group(2) in versions
            else m.group(0)
        ),
        text,
    )


def sync_pins(root, catalog, summary):
    """A chart that pins another chart of the catalog (authelia, the apps of a
    pack, the library) moves with it: its pin follows and it is bumped, which
    can in turn move the charts that pin it, until nothing changes."""
    for _ in range(10):
        versions = catalog_versions(catalog)
        repinned = []
        for name in sorted(versions):
            chart_yaml = os.path.join(catalog, name, "Chart.yaml")
            text = read(chart_yaml)
            new = repin(text, versions)
            if new != text:
                write(chart_yaml, new)
                repinned.append(name)
        if not repinned:
            return
        for name in repinned:
            _, v = bump_chart(root, os.path.join(catalog, name, "Chart.yaml"), summary)
            if v:
                summary.moved.append(f"chart {name} -> {v} (follows what it pins)")
    summary.failed.append("chart pins did not settle after 10 rounds")


CUSTOM_LIBRARY = re.compile(
    r'^(const CUSTOM_LIBRARY_VERSION: &str = )"[^"]+";', re.MULTILINE
)


def pin_custom_library(text, lib):
    new, count = CUSTOM_LIBRARY.subn(lambda m: f'{m.group(1)}"{lib}";', text)
    if count != 1:
        raise RuntimeError(
            f"custom_app.rs has {count} CUSTOM_LIBRARY_VERSION lines, expected 1"
        )
    return new


def bump_charts(root, touched, summary):
    catalog = os.path.join(root, "apps", "catalog")
    if not os.path.isdir(catalog):
        return
    charts = {
        c for c in (chart_of(catalog, p) for p in touched if p.startswith(catalog)) if c
    }
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
        custom = os.path.join(
            root, "homelab", "local-api", "src", "routers", "custom_app.rs"
        )
        write(custom, pin_custom_library(read(custom), lib))
        summary.moved.append(f"chart yolab-common -> {lib} (every app chart follows)")
    for name in sorted(charts):
        _, v = bump_chart(root, os.path.join(catalog, name, "Chart.yaml"), summary)
        if v:
            summary.moved.append(f"chart {name} -> {v}")
    sync_pins(root, catalog, summary)
    arches = os.path.join(catalog, "image_arches.py")
    if charts and os.path.isfile(arches):
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
                    lambda m, target=target: m.group(1) + target,
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


COMMIT = re.compile(r"^[0-9a-f]{40}$")
MAJOR = re.compile(r"^v?(\d+)(?:\.|$)")


def action_major(ref, comment):
    """The major an action is on: from the tag it names, or for a pinned
    commit from the tag written beside it."""
    tag = ref
    if COMMIT.match(ref):
        tag = (comment or "").lstrip(" #").strip()
    m = MAJOR.match(tag)
    return int(m.group(1)) if m else None


def newest_in_major(tags, major):
    """Newest release on this major; any release when the major is unknown."""
    if major is None:
        return newest_release(tags)
    return newest_release(
        [t for t in tags if SEMVER.match(t) and int(SEMVER.match(t).group(1)) == major]
    )


def pin_uses(line, resolve):
    m = USES_LINE.match(line)
    if not m:
        return line
    lead, repo, sub, ref, comment = m.groups()
    found = resolve(repo, action_major(ref, comment))
    if not found:
        return line
    tag, sha = found
    return f"{lead}{repo}{sub}@{sha} # {tag}"


def update_actions(root, summary):
    files = list(
        walk(os.path.join(root, ".github"), lambda p: p.endswith((".yml", ".yaml")))
    )
    remote = {}
    cache = {}

    def resolve(repo, major):
        if repo not in remote:
            try:
                text = run(
                    ["git", "ls-remote", "--tags", f"https://github.com/{repo}"]
                ).decode()
                remote[repo] = parse_ls_remote(text)
            except Exception as e:  # noqa: BLE001
                summary.failed.append(f"action {repo}: {e}")
                remote[repo] = None
        tags = remote[repo]
        if tags is None:
            return None
        key = (repo, major)
        if key not in cache:
            tag = newest_in_major(tags, major)
            cache[key] = (tag, tags[tag]) if tag else None
            newest = newest_release(tags)
            if (
                major is not None
                and newest
                and tag != newest
                and (int(SEMVER.match(newest).group(1)) > major)
            ):
                summary.held.append(
                    f"action {repo}: {newest} exists, a new major line, left for a person"
                )
        return cache[key]

    touched = []
    for path in files:
        text = read(path)
        new_text = "\n".join(pin_uses(line, resolve) for line in text.split("\n"))
        if new_text != text:
            write(path, new_text)
            touched.append(path)
    for (repo, _), found in sorted(cache.items(), key=lambda kv: str(kv[0])):
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
    if json.loads(read(path)) != before:
        # The new inputs (nixpkgs above all) must still build and test
        # everything listed for this repo, or the step puts flake.lock back.
        build_checks(root, repo_checks(root))
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

        def go(crate=crate, rel=rel, lock=lock):
            gate = verify_checks(root, rel)
            # Semver-compatible moves only: `cargo update` never crosses a
            # major, and Cargo.toml is left exactly as written.
            _, notes = run(["cargo", "update", "--verbose"], cwd=crate, stderr=True)
            run(["cargo", "metadata", "--locked", "--format-version", "1"], cwd=crate)
            build_checks(root, gate)
            summary.moved.append(f"cargo {rel}")
            majors = cargo_majors(notes.decode(errors="replace"))
            if majors:
                summary.held.append(
                    f"cargo {rel}: new majors left for a person: {', '.join(majors)}"
                )

        step(summary, f"cargo {rel}", [lock, *manifests], go)


CARGO_UNCHANGED = re.compile(
    r"Unchanged (\S+) v(\d+)\.(\d+)\S* \(available: v(\d+)\.(\d+)\S*\)"
)


def cargo_majors(notes):
    """Direct or transitive crates cargo could not move because only an
    incompatible release is newer (cargo update --verbose says so)."""
    found = []
    for name, maj, mnr, new_maj, new_mnr in CARGO_UNCHANGED.findall(notes):
        old = (int(maj), int(mnr) if maj == "0" else 0)
        new = (int(new_maj), int(new_mnr) if new_maj == "0" else 0)
        if new != old and name not in found:
            found.append(name)
    return found


# The flake outputs that build and test each lockfile's package. An update
# lands only when they all build with the new lock; a lockfile not listed here
# is never updated, so nothing moves without being built and tested.
VERIFY = {
    ("yolab", "homelab/client-ui"): [
        "checks.{system}.client-ui",
        "checks.{system}.client-ui-tests",
        "checks.{system}.client-ui-lint",
    ],
    ("yolab", "homelab/local-api"): [
        "checks.{system}.local-api-tests",
        "checks.{system}.clippy-local-api",
    ],
    ("yolab", "installer/nixos/backend-rs"): [
        "checks.{system}.installer-tests",
        "checks.{system}.clippy-installer",
    ],
    ("yolab", "shells/desktop"): [
        "checks.{system}.desktop-client-tests",
        "checks.{system}.clippy-desktop-client",
    ],
    ("yolab-external", "."): ["checks.{system}.tests", "checks.{system}.clippy"],
    ("yolab-external", "frontend"): ["packages.{system}.frontend"],
}


def verify_checks(root, rel):
    attrs = VERIFY.get((os.path.basename(root), rel))
    if not attrs:
        raise RuntimeError(
            f"no flake check is listed in VERIFY for {rel}; add the ones that "
            "build and test it before it can be updated"
        )
    return attrs


# Built on top of VERIFY when a repo's flake inputs move: what nixpkgs can
# break that no package check covers.
FLAKE_VERIFY = {
    "yolab": ["nixosConfigurations.yolab.config.services.caddy.package"],
}


def repo_checks(root):
    repo = os.path.basename(root)
    attrs = list(FLAKE_VERIFY.get(repo, []))
    for (r, _), listed in VERIFY.items():
        if r == repo:
            attrs += [a for a in listed if a not in attrs]
    return attrs


def current_system():
    return (
        run(["nix", "eval", "--raw", "--impure", "--expr", "builtins.currentSystem"])
        .decode()
        .strip()
    )


HASH_MISMATCH = re.compile(
    r"specified:\s*(sha256-[A-Za-z0-9+/=]+)\s+got:\s*(sha256-[A-Za-z0-9+/=]+)"
)
HASH_REPAIRS = 5


def nix_build(cmd):
    """nix build with its log shown live; returns (ok, log)."""
    log(f"$ {' '.join(cmd)}")
    proc = subprocess.Popen(cmd, stderr=subprocess.PIPE, text=True)
    lines = []
    for line in proc.stderr:
        sys.stderr.write(line)
        lines.append(line)
    proc.wait(timeout=7200)
    return proc.returncode == 0, "".join(lines)


def build_checks(root, attrs):
    """Build every attr. A fixed-output hash pinned in this repo that the new
    inputs changed (a Caddy with plugins after nixpkgs moves Caddy) is
    re-pinned and the build tried again; any other failure stops the step."""
    if not attrs:
        return
    system = current_system()
    cmd = [
        "nix",
        "build",
        "--no-link",
        "--impure",
        *(f"{root}#{a.format(system=system)}" for a in attrs),
    ]
    for _ in range(HASH_REPAIRS + 1):
        ok, out = nix_build(cmd)
        if ok:
            return
        repaired = [
            old
            for old, new in HASH_MISMATCH.findall(out)
            if replace_in_nix(root, old, new)
        ]
        if not repaired:
            tail = " / ".join(out.strip().splitlines()[-3:])
            raise RuntimeError(f"nix build failed: {tail}")
        log(f"re-pinned {len(repaired)} fixed-output hash(es); building again")
    raise RuntimeError(f"hashes still moving after {HASH_REPAIRS} re-pins")


# Stability over novelty: majors are reported, never taken.
NPM_TARGETS = ("minor", "patch")


def first_that_works(targets, attempt, paths):
    errors = []
    for target in targets:
        snap = Snapshot(paths)
        try:
            attempt(target)
            return target
        except Exception as e:  # noqa: BLE001
            snap.restore()
            errors.append(f"{target}: {e}")
    raise RuntimeError("; ".join(errors))


NPM_FETCHER_VERSIONS = ("1", "2")


def npm_deps_hash(lock, version):
    env = {"NPM_FETCHER_VERSION": version}
    return run(["prefetch-npm-deps", lock], timeout=1800, env=env).decode().strip()


def pinned_npm_hash(root, lock):
    """The npmDepsHash a .nix file pins for this lock, and the fetcher
    version (npmDepsFetcherVersion) that produced it."""
    texts = [read(p) for p in walk(root, lambda p: p.endswith(".nix"))]
    for version in NPM_FETCHER_VERSIONS:
        h = npm_deps_hash(lock, version)
        if any(h in t for t in texts):
            return h, version
    return None, None


def replace_in_nix(root, old, new):
    hits = []
    for path in walk(root, lambda p: p.endswith(".nix")):
        text = read(path)
        if old in text:
            write(path, text.replace(old, new))
            hits.append(path)
    return hits


def npm_upgrades(pkg_dir, target):
    out = run(["ncu", "--jsonUpgraded", "--target", target], cwd=pkg_dir)
    return json.loads(out or b"{}")


def npm_majors(pkg_dir):
    """Dependencies whose newest release is a major the minor target skips."""
    latest = npm_upgrades(pkg_dir, "latest")
    minor = npm_upgrades(pkg_dir, "minor")
    return sorted(n for n, v in latest.items() if minor.get(n) != v)


def update_npm(root, summary):
    for lock in walk(root, lambda p: os.path.basename(p) == "package-lock.json"):
        pkg_dir = os.path.dirname(lock)
        rel = os.path.relpath(pkg_dir, root)
        nix_files = list(walk(root, lambda p: p.endswith(".nix")))
        manifest = os.path.join(pkg_dir, "package.json")

        def go(
            pkg_dir=pkg_dir, lock=lock, rel=rel, manifest=manifest, nix_files=nix_files
        ):
            gate = verify_checks(root, rel)
            old_hash, version = pinned_npm_hash(root, lock)
            if old_hash is None:
                raise RuntimeError(
                    f"no .nix file holds the npmDepsHash of {rel}/package-lock.json "
                    "as it stands; fix the pin first"
                )
            majors = npm_majors(pkg_dir)

            def attempt(target):
                run(
                    ["ncu", "--upgrade", "--peer", "--target", target],
                    cwd=pkg_dir,
                    capture=False,
                )
                # --strict-peer-deps: a peer conflict is a failure, never
                # papered over.
                run(
                    [
                        "npm",
                        "install",
                        "--package-lock-only",
                        "--strict-peer-deps",
                        "--ignore-scripts",
                        "--no-audit",
                        "--no-fund",
                    ],
                    cwd=pkg_dir,
                    capture=False,
                )
                # Every package in the tree satisfies every range that asks
                # for it, peers included.
                run(["npm", "ls", "--all", "--package-lock-only"], cwd=pkg_dir)
                new_hash = npm_deps_hash(lock, version)
                if new_hash != old_hash:
                    replace_in_nix(root, old_hash, new_hash)
                build_checks(root, gate)

            landed = first_that_works(
                NPM_TARGETS, attempt, [manifest, lock, *nix_files]
            )
            if landed != NPM_TARGETS[0]:
                summary.held.append(
                    f"npm {rel}: minor updates did not build and test together; "
                    f"took {landed} updates only"
                )
            if majors:
                summary.held.append(
                    f"npm {rel}: new majors left for a person: {', '.join(majors)}"
                )
            summary.moved.append(f"npm {rel}")

        step(summary, f"npm {rel}", [lock, manifest, *nix_files], go)


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
                [
                    os.path.join(root, "flake.lock"),
                    *walk(root, lambda p: p.endswith(".nix")),
                ],
                lambda root=root: update_flake(root, summary),
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
