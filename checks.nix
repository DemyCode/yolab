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
