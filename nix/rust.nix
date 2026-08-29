# The Rust toolchain and both crates, defined once.
#
# There were four copies of the toolchain expression and three of the crane
# setup — flake.nix's devShell, checks.nix, homelab/builds.nix and treefmt.nix
# each built their own from the same rust-toolchain.toml. Identical text, but
# nothing made them stay identical, and one had already drifted:
# installer/nixos/iso-config.nix built the installer with plain
# `inputs.crane.mkLib pkgs` — nixpkgs' rustc, not the pinned one, and without
# the pkg-config that `installer-tests` passes. The binary on the shipped ISO
# was therefore built by a different compiler than the binary the tests ran.
#
# Everything now comes from here, so the devshell, the checks, the packages and
# the ISO cannot disagree about what "the toolchain" is.
{
  pkgs,
  inputs,
}: let
  rustToolchain = (pkgs.extend inputs.rust-overlay.overlays.default)
    .rust-bin.fromRustupToolchainFile ../homelab/local-api/rust-toolchain.toml;

  craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

  mkCrate = {
    pname,
    path,
    nativeBuildInputs ? [],
    buildInputs ? [],
  }: let
    args = {
      inherit pname nativeBuildInputs buildInputs;
      version = "0.1.0";
      src = craneLib.cleanCargoSource (craneLib.path path);
      strictDeps = true;
    };

    # Built once and reused by both the package and the tests. They used to be
    # separate derivations with different pnames — `local-api` from builds.nix
    # and `local-api` from checks.nix — which meant every dependency of this
    # crate compiled twice on a cold cache, once for the binary and once for
    # `cargo test`.
    cargoArtifacts = craneLib.buildDepsOnly args;

    # Coverage keeps its own dependency build. cargo-llvm-cov compiles with
    # instrumentation flags, so it cannot reuse artifacts built without them —
    # sharing them here would either be silently rebuilt or, worse, produce a
    # report with no coverage data for the dependencies' inlined code.
    covArgs = args // {pname = "${pname}-coverage";};
  in {
    inherit args cargoArtifacts;

    package = craneLib.buildPackage (args // {inherit cargoArtifacts;});

    tests = craneLib.cargoTest (args // {inherit cargoArtifacts;});

    coverage = craneLib.cargoLlvmCov (covArgs
      // {
        cargoArtifacts = craneLib.buildDepsOnly covArgs;
        cargoLlvmCovCommand = "test";
        # --summary-only cannot be combined with --html, so the browsable
        # report is produced here and the text summary below reuses the same
        # profile data rather than re-running the suite.
        cargoLlvmCovExtraArgs = "--html --output-dir $out";
        # --release must match the flag the test run used, or `report` looks
        # for profile data in the debug target dir and silently finds none.
        postInstall = ''
          echo "── coverage summary: ${pname} ──"
          cargo llvm-cov report --release --summary-only \
            | tee "$out/coverage-summary.txt"
        '';
      });
  };
in {
  inherit rustToolchain craneLib;

  crates = {
    local-api = mkCrate {
      pname = "local-api";
      path = ../homelab/local-api;
      # llvmPackages.bintools for the profiler runtime coverage shells out to.
      nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.bintools];
      buildInputs = [pkgs.openssl];
    };

    # First-boot provisioning: partitioning, tunnel registration, and the
    # config.toml every later rebuild reads. It only ever runs on a stranger's
    # bare metal, so the tests are the one place it is exercised at all.
    installer = mkCrate {
      pname = "yolab-installer";
      path = ../installer/nixos/backend-rs;
      nativeBuildInputs = [pkgs.pkg-config];
    };
  };
}
