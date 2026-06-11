{
  pkgs,
  lib,
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

  # ── Local API (Rust / Axum management daemon) ────────────────────────────
  localApiEnv = pkgs.rustPlatform.buildRustPackage {
    pname = "local-api";
    version = "0.1.0";
    src = ./local-api;
    cargoLock.lockFile = ./local-api/Cargo.lock;
    nativeBuildInputs = [pkgs.pkg-config];
    buildInputs = [pkgs.openssl];
  };
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
