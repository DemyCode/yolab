{
  pkgs,
  lib,
  inputs,
}:
let
  configPath = ./ignored/config.toml;
  homelabConfig = builtins.fromTOML (builtins.readFile configPath);

  cfg = homelabConfig.homelab;
  tunnelCfg = homelabConfig.tunnel or { };
  nodeCfg = homelabConfig.node or { };

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
    npmDepsHash = "sha256-Vge28xe0Lpft6BGzFsLwx1GKul/xTmNTzk/FAbOdckQ=";
    npmFlags = [ "--legacy-peer-deps" ];
    installPhase = ''
      npm run build
      cp -r dist $out
    '';
  };

  # ── Local API (Python / FastAPI management daemon) ───────────────────────
  localApiWorkspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ./local-api;
  };
  localApiOverlay = localApiWorkspace.mkPyprojectOverlay { sourcePreference = "wheel"; };
  localApiPythonSet =
    (pkgs.callPackage inputs.pyproject-nix.build.packages { python = pkgs.python311; }).overrideScope
      (
        lib.composeManyExtensions [
          inputs.pyproject-build-systems.overlays.wheel
          localApiOverlay
        ]
      );
  localApiEnv = localApiPythonSet.mkVirtualEnv "local-api-env" localApiWorkspace.deps.default;
in
{
  hostname = cfg.hostname;
  timezone = cfg.timezone;
  locale = cfg.locale;
  sshPort = cfg.ssh_port;
  allowedSshKeys = cfg.allowed_ssh_keys or [ ];
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
