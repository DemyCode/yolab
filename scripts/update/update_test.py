import os
import tempfile
import unittest
from unittest import mock

import update


class Tags(unittest.TestCase):
    def test_an_image_follows_its_own_major_line_and_suffix(self):
        tags = ["17.9-alpine", "17.10-alpine", "17.11-bookworm", "17.12", "18.0-alpine"]
        self.assertEqual(update.newer_tag("17.9-alpine", tags), "17.10-alpine")

    def test_a_new_major_of_a_database_is_never_taken(self):
        self.assertIsNone(
            update.newer_tag("17.10-alpine", ["17.10-alpine", "18.0-alpine"])
        )

    def test_the_new_major_is_reported_instead(self):
        tags = ["17.10-alpine", "18.0-alpine", "18.1-alpine"]
        self.assertEqual(update.newest_major("17.10-alpine", tags), "18.1-alpine")

    def test_on_a_zero_major_the_minor_is_the_line(self):
        tags = ["0.35.4", "0.36.0"]
        self.assertEqual(update.newer_tag("0.35.1", tags), "0.35.4")

    def test_calendar_versions_move_across_years(self):
        self.assertEqual(update.newer_tag("2025.10.1", ["2026.1.0"]), "2026.1.0")

    def test_a_v_prefix_is_kept(self):
        self.assertEqual(update.newer_tag("v2.45.1", ["2.46.0", "v2.46.0"]), "v2.46.0")

    def test_the_tag_depth_is_kept(self):
        self.assertIsNone(update.newer_tag("3.21", ["3.22.1"]))
        self.assertEqual(update.newer_tag("3.21", ["3.22", "3.22.1"]), "3.22")

    def test_a_named_tag_only_has_its_digest_refreshed(self):
        self.assertIsNone(update.newer_tag("latest", ["latest", "1.0"]))
        self.assertIsNone(update.newer_tag("alpine", ["alpine", "1.27-alpine"]))

    def test_a_word_prefix_is_kept(self):
        tags = ["smb-amd64", "smb-debian-20261110", "smb-20261109", "smb-20261110"]
        self.assertEqual(update.newer_tag("smb-20231122", tags), "smb-20261110")

    def test_a_single_number_tag_stays(self):
        self.assertIsNone(update.newer_tag("2", ["2", "3"]))

    def test_an_operator_chart_only_takes_patches(self):
        self.assertEqual(
            update.newest_patch("v1.16.3", ["v1.16.7", "v1.17.0", "v1.16.4"]), "v1.16.7"
        )
        self.assertIsNone(update.newest_patch("0.16.0", ["0.17.0"]))

    def test_a_release_is_never_a_prerelease(self):
        self.assertEqual(
            update.newest_release(["v4.2.0", "v5.0.0-rc.1", "v4.10.1", "latest"]),
            "v4.10.1",
        )

    def test_a_patch_bump(self):
        self.assertEqual(update.bump_patch("0.1.9"), "0.1.10")


class References(unittest.TestCase):
    def test_a_registry_port_is_not_a_tag(self):
        self.assertEqual(
            update.split_ref("reg:5000/a/b:1.2@sha256:" + "a" * 64),
            ("reg:5000/a/b", "1.2", "sha256:" + "a" * 64),
        )
        self.assertEqual(update.split_ref("reg:5000/a/b"), ("reg:5000/a/b", None, None))

    def test_refs_are_found_where_they_are_unambiguous(self):
        digest = "sha256:" + "b" * 64
        text = "\n".join(
            [
                f'default "caddy:2@{digest}"',
                "        image: nginx:alpine",
                '  - image: "redis:8.4"',
                "        image: {{ .Values.image }}",
                "FROM alpine:3.21 AS base",
                "FROM base",
                "FROM rust:${RUST}",
                "        image: ghcr.io/demycode/wg-sidecar:latest",
            ]
        )
        self.assertEqual(
            update.image_refs(text),
            {f"caddy:2@{digest}", "nginx:alpine", "redis:8.4", "alpine:3.21"},
        )

    def test_a_replacement_never_touches_a_longer_name(self):
        text = "image: nginx:alpine\nimage: nginx:alpine-slim\n"
        new = update.replace_refs(
            text, {"nginx:alpine": "nginx:alpine@sha256:" + "c" * 64}
        )
        self.assertEqual(
            new, f"image: nginx:alpine@sha256:{'c' * 64}\nimage: nginx:alpine-slim\n"
        )

    def test_a_pinned_ref_is_replaced_whole(self):
        old = "postgres:17.9@sha256:" + "d" * 64
        new = "postgres:17.10@sha256:" + "e" * 64
        self.assertEqual(
            update.replace_refs(f'image: "{old}"', {old: new}), f'image: "{new}"'
        )


class Actions(unittest.TestCase):
    def test_an_annotated_tag_resolves_to_its_commit(self):
        text = (
            "aaa\trefs/tags/v5.0.0\nbbb\trefs/tags/v5.0.0^{}\nccc\trefs/tags/v4.0.0\n"
        )
        self.assertEqual(
            update.parse_ls_remote(text), {"v5.0.0": "bbb", "v4.0.0": "ccc"}
        )

    def test_an_action_is_pinned_to_a_commit_with_its_tag_beside_it(self):
        line = "      - uses: actions/checkout@v5"
        self.assertEqual(
            update.pin_uses(line, lambda repo: ("v5.0.1", "f" * 40)),
            f"      - uses: actions/checkout@{'f' * 40} # v5.0.1",
        )

    def test_a_pinned_action_moves_and_its_comment_follows(self):
        line = f"        uses: github/codeql-action/init@{'a' * 40} # v3.1.0"
        self.assertEqual(
            update.pin_uses(line, lambda repo: ("v3.2.0", "b" * 40)),
            f"        uses: github/codeql-action/init@{'b' * 40} # v3.2.0",
        )

    def test_local_and_docker_actions_are_left_alone(self):
        for line in (
            "      - uses: ./.github/actions/x",
            "      - uses: docker://alpine:3",
        ):
            self.assertEqual(
                update.pin_uses(line, lambda repo: ("v1.0.0", "a" * 40)), line
            )


class Flake(unittest.TestCase):
    def test_path_inputs_are_never_updated(self):
        lock = {
            "root": "root",
            "nodes": {
                "root": {
                    "inputs": {"nixpkgs": "nixpkgs", "yolab-machine": "yolab-machine"}
                },
                "nixpkgs": {"locked": {"type": "github", "rev": "a"}},
                "yolab-machine": {"locked": {"type": "path"}},
            },
        }
        self.assertEqual(update.flake_inputs(lock), ["nixpkgs"])

    def problem(self, k3s, ceph_cached, needs_ceph=True):
        def attr(rev, name):
            if name == "k3s.version":
                return k3s[0] if rev == "old" else k3s[1]
            return "/nix/store/x-ceph"

        with (
            mock.patch.object(update, "nixpkgs_attr", attr),
            mock.patch.object(update, "is_cached", lambda path: ceph_cached),
        ):
            return update.nixpkgs_problem("old", "new", needs_ceph)

    def test_k3s_may_move_one_minor(self):
        self.assertIsNone(self.problem(("1.33.4+k3s1", "1.34.1+k3s1"), True))

    def test_k3s_may_not_skip_a_minor(self):
        self.assertIn(
            "1.33 -> 1.35", self.problem(("1.33.4+k3s1", "1.35.0+k3s1"), True)
        )

    def test_a_ceph_that_failed_to_build_upstream_holds_nixpkgs(self):
        self.assertIn("ceph", self.problem(("1.33.4", "1.33.5"), False))

    def test_a_repo_without_ceph_does_not_wait_for_it(self):
        self.assertIsNone(self.problem(("1.33.4", "1.33.5"), False, needs_ceph=False))


class Charts(unittest.TestCase):
    def test_every_app_follows_a_new_library_version(self):
        text = (
            "version: 0.1.4\ndependencies:\n  - name: yolab-common\n"
            '    version: "0.1.13"\n    repository: oci://x\n'
        )
        new = update.LIB_DEP.sub(lambda m: f'{m.group(1)}"0.1.14"', text)
        self.assertIn('version: "0.1.14"', new)
        self.assertEqual(update.TOP_VERSION.search(new).group(1), "0.1.4")


class Rollback(unittest.TestCase):
    def test_a_failed_step_leaves_its_files_as_they_were(self):
        with tempfile.TemporaryDirectory() as tmp:
            kept = os.path.join(tmp, "Cargo.lock")
            created = os.path.join(tmp, "new")
            update.write(kept, "before")

            def fail():
                update.write(kept, "half")
                update.write(created, "x")
                raise RuntimeError("network down")

            summary = update.Summary()
            update.step(summary, "cargo", [kept, created], fail)
            self.assertEqual(update.read(kept), "before")
            self.assertFalse(os.path.exists(created))
            self.assertEqual(summary.failed, ["cargo: network down"])


class Cargo(unittest.TestCase):
    def test_only_majors_are_reported_as_held(self):
        notes = """
    Updating crates.io index
     Locking 0 packages to highest compatible versions
   Unchanged tauri v2.9.1 (available: v3.0.0)
   Unchanged rand v0.8.5 (available: v0.9.2)
   Unchanged url v2.5.4 (available: v2.5.7)
   Unchanged tinyvec v0.3.1 (available: v0.3.4)
"""
        self.assertEqual(update.cargo_majors(notes), ["tauri", "rand"])

    def test_a_lock_without_checks_is_never_updated(self):
        with self.assertRaises(RuntimeError):
            update.verify_checks("/x/yolab", "some/new/crate")

    def test_every_lock_in_yolab_is_built_and_tested(self):
        for rel in (
            "homelab/local-api",
            "shells/desktop",
            "installer/nixos/backend-rs",
        ):
            self.assertTrue(update.verify_checks("/x/yolab", rel))


class NpmTargets(unittest.TestCase):
    def test_a_major_is_never_taken(self):
        self.assertNotIn("latest", update.NPM_TARGETS)
        self.assertEqual(update.NPM_TARGETS[0], "minor")

    def test_minor_wins_when_it_builds(self):
        tried = []
        landed = update.first_that_works(update.NPM_TARGETS, tried.append, [])
        self.assertEqual(landed, "minor")
        self.assertEqual(tried, ["minor"])

    def test_a_broken_minor_falls_back_to_patch_with_the_files_as_they_were(self):
        with tempfile.TemporaryDirectory() as tmp:
            manifest = os.path.join(tmp, "package.json")
            nix = os.path.join(tmp, "builds.nix")
            update.write(manifest, "before")
            update.write(nix, "hash-before")

            def attempt(target):
                update.write(manifest, target)
                update.write(nix, f"hash-{target}")
                if target == "minor":
                    raise RuntimeError("ERESOLVE")

            landed = update.first_that_works(
                update.NPM_TARGETS, attempt, [manifest, nix]
            )
            self.assertEqual(landed, "patch")
            self.assertEqual(update.read(manifest), "patch")
            self.assertEqual(update.read(nix), "hash-patch")

    def test_when_nothing_installs_every_reason_is_kept(self):
        def attempt(target):
            raise RuntimeError(f"no {target}")

        with self.assertRaises(RuntimeError) as caught:
            update.first_that_works(("minor", "patch"), attempt, [])
        self.assertIn("minor: no minor", str(caught.exception))
        self.assertIn("patch: no patch", str(caught.exception))

    def test_majors_are_those_the_minor_target_skips(self):
        found = {
            "latest": {"react": "^20.0.0", "clsx": "^2.1.2"},
            "minor": {"clsx": "^2.1.2"},
        }
        with mock.patch.object(update, "npm_upgrades", lambda _d, t: found[t]):
            self.assertEqual(update.npm_majors("."), ["react"])


class DisabledCharts(unittest.TestCase):
    def test_a_disabled_charts_images_are_not_followed(self):
        with tempfile.TemporaryDirectory() as root:
            for name, ann in (("on", ""), ("off", '  yolab.io/disabled: "gone"\n')):
                d = os.path.join(root, "apps", "catalog", name)
                os.makedirs(os.path.join(d, "templates"))
                update.write(
                    os.path.join(d, "Chart.yaml"),
                    f"name: {name}\nannotations:\n{ann}  yolab.io/icon: x\n",
                )
                update.write(os.path.join(d, "templates", "app.yaml"), "image: a:1\n")
            files = [os.path.relpath(p, root) for p in update.image_files(root)]
            self.assertIn("apps/catalog/on/templates/app.yaml", files)
            self.assertFalse([f for f in files if "/off/" in f])


class CustomLibrary(unittest.TestCase):
    def test_custom_services_follow_a_yolab_common_bump(self):
        text = 'use x;\nconst CUSTOM_LIBRARY_VERSION: &str = "0.1.36";\nfn f() {}\n'
        self.assertEqual(
            update.pin_custom_library(text, "0.1.37"),
            'use x;\nconst CUSTOM_LIBRARY_VERSION: &str = "0.1.37";\nfn f() {}\n',
        )

    def test_a_file_without_the_pin_stops_the_update(self):
        with self.assertRaises(RuntimeError):
            update.pin_custom_library("fn f() {}\n", "0.1.37")


class NpmPin(unittest.TestCase):
    def test_the_pin_is_matched_under_the_fetcher_version_that_made_it(self):
        with tempfile.TemporaryDirectory() as root:
            with open(os.path.join(root, "builds.nix"), "w") as f:
                f.write('npmDepsFetcherVersion = 2;\nnpmDepsHash = "sha256-v2";\n')
            lock = os.path.join(root, "package-lock.json")
            with mock.patch.object(
                update, "npm_deps_hash", side_effect=lambda _l, v: f"sha256-v{v}"
            ):
                self.assertEqual(update.pinned_npm_hash(root, lock), ("sha256-v2", "2"))

    def test_an_unpinned_lock_is_reported(self):
        with tempfile.TemporaryDirectory() as root:
            lock = os.path.join(root, "package-lock.json")
            with mock.patch.object(update, "npm_deps_hash", return_value="sha256-x"):
                self.assertEqual(update.pinned_npm_hash(root, lock), (None, None))


if __name__ == "__main__":
    unittest.main()
