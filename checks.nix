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
# `nix flake check` runs these too, but it additionally evaluates
# nixosConfigurations, which needs homelab/ignored/config.toml — untracked, and
# absent on a fresh clone. `nix run .#ci` is the entry point that works
# everywhere.
#
# Note: nix builds from the git index, not the working directory, so a brand new
# file is invisible to these until `git add -N` (or a real `git add`). Editing a
# file that is already tracked needs nothing.
#
# THE NIXOS MODULES ARE NOT COVERED HERE, and that is a real gap. Evaluating
# them needs a config.toml at a fixed path, so it cannot be a pure derivation
# without a refactor of homelab/shared.nix. Until then, both halves of the
# cluster have to be checked by hand — and BOTH matter, because creating a
# cluster and joining one are different code paths in k3s and in Ceph:
#
#   # only on a machine with no real config.toml to lose:
#   cp homelab/ci-config.toml      homelab/ignored/config.toml   # creates
#   nix build --no-link path:.#nixosConfigurations.yolab.config.system.build.toplevel
#   cp homelab/ci-join-config.toml homelab/ignored/config.toml   # joins
#   nix build --no-link path:.#nixosConfigurations.yolab.config.system.build.toplevel
#   rm homelab/ignored/config.toml
#
# NEVER run that on a node: it overwrites the machine's real config.toml, which
# is untracked and holds secrets that exist nowhere else.
{
  pkgs,
  inputs,
}: let
  rustToolchain = (pkgs.extend inputs.rust-overlay.overlays.default)
    .rust-bin.fromRustupToolchainFile ./homelab/local-api/rust-toolchain.toml;
  craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

  # Builds `cargo test` for a crate as a derivation, reusing a dependency-only
  # build so a source change does not rebuild the world.
  cargoTests = {
    name,
    path,
    nativeBuildInputs ? [],
    buildInputs ? [],
  }: let
    args = {
      pname = name;
      version = "0.1.0";
      src = craneLib.cleanCargoSource (craneLib.path path);
      strictDeps = true;
      inherit nativeBuildInputs buildInputs;
    };
  in
    craneLib.cargoTest (args
      // {
        cargoArtifacts = craneLib.buildDepsOnly args;
      });
  # Line/region coverage for a crate, as a browsable HTML report.
  #
  # Deliberately NOT one of the checks below: it is a tool for looking at where
  # the tests are thin, not a gate. Wiring it into CI would either fail builds on
  # an arbitrary percentage or, worse, invite writing tests that move the number
  # rather than tests that catch bugs.
  #
  #   nix run .#coverage           # build both reports and print where they are
  #   nix build .#coverage-local-api
  cargoCoverage = {
    name,
    path,
    nativeBuildInputs ? [],
    buildInputs ? [],
  }: let
    args = {
      pname = "${name}-coverage";
      version = "0.1.0";
      src = craneLib.cleanCargoSource (craneLib.path path);
      strictDeps = true;
      inherit nativeBuildInputs buildInputs;
    };
  in
    craneLib.cargoLlvmCov (args
      // {
        cargoArtifacts = craneLib.buildDepsOnly args;
        cargoLlvmCovCommand = "test";
        # --summary-only cannot be combined with --html, so the browsable report
        # is produced here and the text summary below reuses the same profile
        # data rather than re-running the suite.
        cargoLlvmCovExtraArgs = "--html --output-dir $out";
        # --release must match the flag the test run used, or `report` looks for
        # profile data in the debug target dir and silently finds none.
        postInstall = ''
          echo "── coverage summary: ${name} ──"
          cargo llvm-cov report --release --summary-only \
            | tee "$out/coverage-summary.txt"
        '';
      });
in {
  # ── Rust ────────────────────────────────────────────────────────────────────

  local-api-tests = cargoTests {
    name = "local-api";
    path = ./homelab/local-api;
    nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.bintools];
    buildInputs = [pkgs.openssl];
  };

  # First-boot provisioning: partitioning, tunnel registration, and the
  # config.toml every later rebuild reads. It only ever runs on a stranger's bare
  # metal, so this is the one place it is exercised at all.
  installer-tests = cargoTests {
    name = "yolab-installer";
    path = ./installer/nixos/backend-rs;
    nativeBuildInputs = [pkgs.pkg-config];
  };

  # ── wg-register ─────────────────────────────────────────────────────────────
  #
  # Runs on every app install and every app restart. Driven under busybox sh
  # against stubbed curl/wg, because busybox sh is what the Alpine image actually
  # uses and it is not the same shell as bash-in-POSIX-mode.
  wg-register-tests =
    pkgs.runCommand "wg-register-tests" {
      nativeBuildInputs = [pkgs.busybox pkgs.jq];
      src = ./apps/wg-register;
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
      src = ./apps/catalog;
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

  coverage-local-api = cargoCoverage {
    name = "local-api";
    path = ./homelab/local-api;
    nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.bintools];
    buildInputs = [pkgs.openssl];
  };

  coverage-installer = cargoCoverage {
    name = "yolab-installer";
    path = ./installer/nixos/backend-rs;
    nativeBuildInputs = [pkgs.pkg-config];
  };

  # ── Static analysis ─────────────────────────────────────────────────────────

  shellcheck =
    pkgs.runCommand "shellcheck" {
      nativeBuildInputs = [pkgs.shellcheck];
      src = ./apps;
    } ''
      cp -r "$src" ./apps
      # -x so sourced files are followed; the wg-* scripts run as POSIX sh.
      find ./apps -name '*.sh' -print0 | xargs -0 shellcheck -s sh -x
      touch $out
    '';

  # Not included: a repo-wide `alejandra --check`. It passes for everything added
  # here, but three pre-existing files (homelab/nixos/{configuration,disk-config,
  # common}.nix) would need reformatting first, and that is a large diff with
  # nothing to do with correctness. Add it whenever that reformat is wanted:
  #
  #   nix-fmt = pkgs.runCommand "nix-fmt" { nativeBuildInputs = [pkgs.alejandra]; }
  #     '' alejandra --check ${./.}; touch $out '';
}
