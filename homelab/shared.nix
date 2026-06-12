{
  pkgs,
  inputs,
}: let
  configPath = ./ignored/config.toml;
  homelabConfig = builtins.fromTOML (builtins.readFile configPath);

  cfg = homelabConfig.homelab;
  tunnelCfg = homelabConfig.tunnel or {};
  nodeCfg = homelabConfig.node or {};

  # Whether the tunnel section is populated (installer has run pairing).
  tunnelEnabled = (tunnelCfg.sub_ipv6 or "") != "";

  # The /112 private subnet covering all nodes' cluster IPs.
  # Stored by the installer in config.toml so all nodes share the same value.
  # Falls back to a sensible ULA default if absent (e.g. dev/WSL).
  privateSubnet = tunnelCfg.sub_ipv6_private_subnet or "fd00:cafe::/112";

  # ── UI (React app bundled at NixOS build time) ───────────────────────────
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

  # ── Local API (Rust / Axum via crane) ────────────────────────────────────
  # crane splits the build into two derivations:
  #   localApiDeps — all crate dependencies, cached until Cargo.toml/Cargo.lock changes
  #   localApiEnv  — only your source code, rebuilt on each code change (fast)
  #
  # The toolchain is read from rust-toolchain.toml so pinning/upgrading Rust
  # only requires editing that file — no Nix changes needed.
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
  inherit (cfg) hostname;
  inherit (cfg) timezone;
  inherit (cfg) locale;
  sshPort = cfg.ssh_port;
  allowedSshKeys = cfg.allowed_ssh_keys or [];
  rootSshKey = cfg.root_ssh_key or "";
  homelabPasswordHash = cfg.homelab_password_hash or "";

  inherit
    tunnelCfg
    nodeCfg
    tunnelEnabled
    privateSubnet
    clientUi
    localApiEnv
    ;
}
