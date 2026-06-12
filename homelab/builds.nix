{
  pkgs,
  inputs,
}: let
  clientUi = pkgs.buildNpmPackage {
    pname = "client-ui";
    version = "0.1.0";
    src = ./client-ui;
    npmDepsFetcherVersion = 2;
    npmDepsHash = "sha256-cyxr2ViRgiaueoCTNi4yGvECvNOjtIO2y5Yp7zDXfNc=";
    npmFlags = ["--legacy-peer-deps"];
    installPhase = ''
      npm run build
      cp -r dist $out
    '';
  };

  rustToolchain = (pkgs.extend inputs.rust-overlay.overlays.default)
    .rust-bin.fromRustupToolchainFile ./local-api/rust-toolchain.toml;
  craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;
  localApiSrc = craneLib.cleanCargoSource (craneLib.path ./local-api);
  localApiArgs = {
    src = localApiSrc;
    strictDeps = true;
    nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.bintools];
    buildInputs = [pkgs.openssl];
  };
  localApiDeps = craneLib.buildDepsOnly localApiArgs;
  localApiEnv = craneLib.buildPackage (localApiArgs
    // {
      cargoArtifacts = localApiDeps;
    });
in {
  inherit clientUi localApiEnv;
}
