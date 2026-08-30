# The Rust toolchain and both crates, defined once — the checks, the packages,
# the devshell, the formatter and the ISO all read from here, so none of them
# can end up on a different rustc than the others.
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
      # Registry crates get --cap-lints allow from cargo, so this only binds
      # our own code.
      RUSTFLAGS = "-D warnings";
    };

    cargoArtifacts = craneLib.buildDepsOnly args;

    # Coverage needs its own dependency build: cargo-llvm-cov compiles with
    # instrumentation flags, so artifacts built without them either get
    # rebuilt or produce a report missing the dependencies' inlined code.
    covArgs = args // {pname = "${pname}-coverage";};
  in {
    inherit args cargoArtifacts;

    package = craneLib.buildPackage (args // {inherit cargoArtifacts;});
    tests = craneLib.cargoTest (args // {inherit cargoArtifacts;});

    clippy = craneLib.cargoClippy (args
      // {
        inherit cargoArtifacts;
        pname = "${pname}-clippy";
        cargoClippyExtraArgs = "--all-targets -- -D warnings";
      });

    coverage = craneLib.cargoLlvmCov (covArgs
      // {
        cargoArtifacts = craneLib.buildDepsOnly covArgs;
        cargoLlvmCovCommand = "test";
        # --summary-only cannot be combined with --html, so the report is built
        # here and the summary below reuses the same profile data.
        cargoLlvmCovExtraArgs = "--html --output-dir $out";
        # --release must match the test run, or `report` looks in the debug
        # target dir and silently finds nothing.
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
      # bintools for the profiler runtime coverage shells out to.
      nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.bintools];
      buildInputs = [pkgs.openssl];
    };

    # Only ever runs on a stranger's bare metal, so its tests are the one place
    # it is exercised at all.
    installer = mkCrate {
      pname = "yolab-installer";
      path = ../installer/nixos/backend-rs;
      nativeBuildInputs = [pkgs.pkg-config];
    };
  };
}
