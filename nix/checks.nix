# Every check the project has, as nix derivations.
#
# The point is that CI runs nothing GitHub-specific. One command runs exactly
# these, byte-for-byte the same on a laptop as on a runner — same rust
# toolchain, same helm, same shell, all pinned by flake.lock — so "passes
# locally, red in CI" stops being a category of problem. The GitHub workflow is
# a hook that calls into here and holds no logic of its own.
#
#   nix run .#ci                                      # everything
#   nix build .#checks.x86_64-linux.local-api-tests   # just one
#
# `nix flake check` runs these too. It used to be unusable on a fresh clone,
# because it additionally evaluates nixosConfigurations and those read
# homelab/ignored/config.toml — untracked, and absent anywhere but a real node.
# The config path is now threaded in from flake.nix instead of hardcoded, so
# the machine configs evaluate from the committed CI stubs and `yolab` itself
# is only defined when a real config.toml is present. Both commands work
# everywhere now.
#
# Note: nix builds from the git index, not the working directory, so a brand new
# file is invisible to these until `git add -N` (or a real `git add`). Editing a
# file that is already tracked needs nothing.
#
# The NixOS modules ARE covered here now — see `nixos-create` and `nixos-join`
# below. Both matter, because creating a cluster and joining one are different
# code paths in k3s and in Ceph.
#
# What this replaces was a hand-run ritual that copied a CI stub over
# homelab/ignored/config.toml and rebuilt against it. It carried a "NEVER run
# this on a node" warning, because the file it overwrote holds secrets that
# exist nowhere else — a check nobody could run safely on the machine they were
# working on is a check nobody ran.
{
  pkgs,
  treefmtEval,
  rust,
  nixosSystems,
}: let
  # The same derivation the NixOS module serves from Caddy, reused as a check.
  # Building it IS the test: its installPhase runs `npm run build`, which is
  # `tsc -b && vite build`.
  builds = import ../homelab/builds.nix {inherit pkgs rust;};

  # The toolchain, the crane setup and both crates' build inputs live in
  # rust.nix — the checks, the packages, the devshell and the ISO all read the
  # same definitions from there. `.tests` is `cargo test` as a derivation,
  # reusing a dependency-only build so a source change does not rebuild the
  # world.
  inherit (rust) crates;

  # The whole tree, gitignore-filtered. Reused by every check below that needs
  # to look at more than one directory.
  #
  # Filtered rather than passed raw because `path:.` — the ref this repo tells
  # you to use, since it is the one that reads the gitignored config.toml —
  # hands over the working directory as it sits: 7.5G of target/ and
  # node_modules on any machine that has built the workspace, plus
  # homelab/ignored/config.toml. Reusing .gitignore rather than a second
  # hand-written list means the two cannot disagree about what is source.
  treeSrc =
    pkgs.nix-gitignore.gitignoreRecursiveSource [
      ".git/"
      # Named explicitly as well as being gitignored.
      #
      # .gitignore alone is not enough here, and the failure mode is severe: in
      # the sibling yolab-external repo the identical filter excluded target/
      # when applied to the working directory and did NOT when applied to the
      # flake source under `path:.`, so 5.4G of build tree was copied into the
      # store once per check until the disk hit 100%. The two target/ trees here
      # are 7.5G together. This is the one exclusion that cannot be allowed to
      # depend on a gitignore-to-regex translation being right.
      "target/"
      "node_modules/"
      "result"
      "result-*"
    ]
    ../.;

  # A machine config built end to end, as a check. Evaluating these is most of
  # the value: it is what catches an option that moved, a module that stopped
  # importing, or a config.toml key a module reads but the installer never
  # writes.
  toplevel = name: nixosSystems.${name}.config.system.build.toplevel;
in {
  # ── The client ──────────────────────────────────────────────────────────────
  #
  # `nix run .#ci` used to pass while the UI did not build. The derivation
  # existed the whole time — homelab-ui, the very bytes Caddy serves — but it
  # was only a package, and nothing built packages during a check run. A commit
  # that broke the front end went green.
  #
  # `npx tsc --noEmit` is not a substitute and did not catch the case that
  # prompted this: it runs against tsconfig.app.json alone, while `npm run
  # build` runs `tsc -b`, which builds every project in the solution with the
  # settings each declares — including the unused-locals rule that flagged an
  # orphaned import.
  #
  # This is the shipped artifact rather than a re-implementation of its build,
  # so the check and the deployed bundle cannot drift apart.
  client-ui = builds.clientUi;

  # ── Rust ────────────────────────────────────────────────────────────────────

  local-api-tests = crates.local-api.tests;
  installer-tests = crates.installer.tests;

  # ── wg-register ─────────────────────────────────────────────────────────────
  #
  # Runs on every app install and every app restart. Driven under busybox sh
  # against stubbed curl/wg, because busybox sh is what the Alpine image actually
  # uses and it is not the same shell as bash-in-POSIX-mode.
  wg-register-tests =
    pkgs.runCommand "wg-register-tests" {
      nativeBuildInputs = [pkgs.busybox pkgs.jq];
      src = ../apps/wg-register;
    } ''
      cp -r "$src" ./wg-register
      chmod -R +w ./wg-register
      busybox sh ./wg-register/setup_test.sh
      touch $out
    '';

  # ── Helm charts ─────────────────────────────────────────────────────────────
  #
  # `helm lint` accepts charts that cannot run — three shipped that way. This
  # renders each chart against the yolab-common in this tree (not the published
  # one, so a library change is checked before release) and asserts the result
  # describes a workload that can actually start.
  charts =
    pkgs.runCommand "chart-checks" {
      nativeBuildInputs = [
        pkgs.kubernetes-helm
        (pkgs.python3.withPackages (ps: [ps.pyyaml]))
      ];
      src = ../apps/catalog;
    } ''
      cp -r "$src" ./catalog
      chmod -R +w ./catalog
      # helm insists on a writable home for its cache/config, and there is none
      # in the build sandbox.
      export HOME=$PWD/home
      mkdir -p "$HOME"
      python3 ./catalog/check_charts.py
      touch $out
    '';

  # ── Coverage ────────────────────────────────────────────────────────────────
  #
  # Named with a `coverage-` prefix, which flake.nix uses to keep them OUT of
  # `checks` (so `nix flake check` and `nix run .#ci` stay fast and stay about
  # correctness) while still exposing them as buildable packages.

  coverage-local-api = crates.local-api.coverage;
  coverage-installer = crates.installer.coverage;

  # ── Machine configs ─────────────────────────────────────────────────────────
  #
  # Both halves of the cluster, built from the CI stubs committed next to them.
  # Creating a cluster and joining one are different code paths in k3s and in
  # Ceph, and nothing else in this file touches either of them.
  #
  # These are the only checks whose config.toml is not the real one, which is
  # the entire point: the path is threaded in from flake.nix, so a stub can be
  # used without a node ever having its own config.toml overwritten.
  nixos-create = toplevel "yolab-ci";
  nixos-join = toplevel "yolab-ci-join";

  # WSL shares common.nix with the two above but overrides the platform, the
  # flake target and the repo path, so it is a genuinely different evaluation.
  nixos-wsl = toplevel "yolab-wsl";

  # ── Formatting ──────────────────────────────────────────────────────────────
  #
  # The gate behind `nix fmt`. Built from the same treefmt module the formatter
  # is, so a file this rejects is a file `nix fmt` fixes — there is no second
  # opinion to reconcile, and no way for the two to drift as versions move.
  formatting = treefmtEval.config.build.check treeSrc;

  # ── Static analysis ─────────────────────────────────────────────────────────
  #
  # These three used to exist only as pre-commit hooks, and the pre-commit job
  # in .github/workflows/push.yml is commented out — so on a machine without the
  # hook installed, nothing ran them at all. As derivations they run wherever
  # `nix run .#ci` runs.
  #
  # All three read the source in place. They used to `cp -r` the whole tree into
  # the build directory and chmod it writable first, which a read-only linter
  # has no use for — that is an extra copy of the tree per check, and the
  # sibling repo's CI ran out of disk doing exactly this sort of thing.

  # Every shell script in the tree, not just apps/. installer/macos/install.sh
  # was checked by nothing before this.
  #
  # No `-s sh`: that forced one dialect on every file and made install.sh — a
  # `#!/usr/bin/env bash` script using `&>` and `set -o pipefail` — fail as
  # POSIX. shellcheck reads the shebang, which is the thing that actually
  # decides what interpreter runs the script.
  shellcheck =
    pkgs.runCommand "shellcheck" {
      nativeBuildInputs = [pkgs.shellcheck];
    } ''
      # -x so sourced files are followed.
      find ${treeSrc} -name '*.sh' -print0 | xargs -0 shellcheck -x
      touch $out
    '';

  hadolint =
    pkgs.runCommand "hadolint" {
      nativeBuildInputs = [pkgs.hadolint];
    } ''
      # DL3018 wants every apk package pinned. These images track upstream
      # Alpine deliberately, and the wg-* tools have to match the host kernel's
      # WireGuard, so pinning here buys a stale userland rather than safety.
      find ${treeSrc} -name 'Dockerfile' -print0 \
        | xargs -0 hadolint --ignore DL3018
      touch $out
    '';

  # Dead code in nix. This caught two unused lambda patterns the moment it was
  # switched on — `inputs` in homelab/builds.nix and `pkgs` in nix/treefmt.nix,
  # both left behind when arguments were threaded through rather than rebuilt
  # locally. That is exactly the class of thing that rots quietly.
  #
  # statix is deliberately NOT here. Its remaining findings are all "avoid
  # repeated keys in attribute sets", which is a style preference — 39 of them
  # across 9 files, and flattening `boot.loader.grub.*` into a nested block is
  # not obviously an improvement. Run it by hand: `nix run nixpkgs#statix check`.
  deadnix =
    pkgs.runCommand "deadnix" {
      nativeBuildInputs = [pkgs.deadnix];
    } ''
      deadnix --fail ${treeSrc}
      touch $out
    '';

  # The repo-wide `alejandra --check` that used to be deferred here is now part
  # of `formatting` above, which treefmt runs over every .nix file in the tree.
  # The three files that were holding it back — homelab/nixos/{configuration,
  # disk-config,common}.nix — turned out to be alejandra-clean already.
}
