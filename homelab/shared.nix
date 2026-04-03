{
  pkgs,
  lib,
  inputs,
}:
let
  configPath = ./ignored/config.toml;
  homelabConfig = builtins.fromTOML (builtins.readFile configPath);

  cfg = homelabConfig.homelab;
  sysCfg = homelabConfig.system;
  tunnelCfg = homelabConfig.tunnel;
  swarmCfg = homelabConfig.swarm;
  nodeCfg = homelabConfig.node;

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

  nodeAgentWorkspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ../node-agent;
  };
  nodeAgentOverlay = nodeAgentWorkspace.mkPyprojectOverlay { sourcePreference = "wheel"; };
  nodeAgentPythonSet =
    (pkgs.callPackage inputs.pyproject-nix.build.packages { python = pkgs.python311; }).overrideScope
      (
        lib.composeManyExtensions [
          inputs.pyproject-build-systems.overlays.wheel
          nodeAgentOverlay
        ]
      );
  nodeAgentEnv = nodeAgentPythonSet.mkVirtualEnv "node-agent-env" nodeAgentWorkspace.deps.default;
in
{
  hostname = cfg.hostname;
  timezone = cfg.timezone;
  locale = cfg.locale;
  sshPort = cfg.ssh_port;
  allowedSshKeys = cfg.allowed_ssh_keys;
  rootSshKey = cfg.root_ssh_key;
  homelabPasswordHash = cfg.homelab_password_hash;
  flakeTarget = sysCfg.flake_target;
  repoPath = sysCfg.repo_path;
  inherit
    tunnelCfg
    clientUi
    localApiEnv
    nodeAgentEnv
    swarmCfg
    nodeCfg
    ;
}
