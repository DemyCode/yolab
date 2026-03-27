{ pkgs, lib, inputs }:
let
  configPath = ./ignored/config.toml;
  homelabConfig =
    if builtins.pathExists configPath then
      builtins.fromTOML (builtins.readFile configPath)
    else
      { };

  cfg = homelabConfig.homelab or { };
  sysCfg = homelabConfig.system or { };
  tunnelCfg = homelabConfig.tunnel or { };
  tunnelEnabled = tunnelCfg.enabled or false;

  wgSubnet = lib.optionalString tunnelEnabled (
    (lib.head (lib.splitString "::" tunnelCfg.sub_ipv6)) + "::/64"
  );

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
      (lib.composeManyExtensions [
        inputs.pyproject-build-systems.overlays.wheel
        localApiOverlay
      ]);
  localApiEnv = localApiPythonSet.mkVirtualEnv "local-api-env" localApiWorkspace.deps.default;

  nodeAgentWorkspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ../node-agent;
  };
  nodeAgentOverlay = nodeAgentWorkspace.mkPyprojectOverlay { sourcePreference = "wheel"; };
  nodeAgentPythonSet =
    (pkgs.callPackage inputs.pyproject-nix.build.packages { python = pkgs.python311; }).overrideScope
      (lib.composeManyExtensions [
        inputs.pyproject-build-systems.overlays.wheel
        nodeAgentOverlay
      ]);
  nodeAgentEnv = nodeAgentPythonSet.mkVirtualEnv "node-agent-env" nodeAgentWorkspace.deps.default;

  swarmCfg = homelabConfig.swarm or { };
  swarmEnabled = swarmCfg.enabled or false;
  nodeCfg = homelabConfig.node or { };
in
{
  hostname = cfg.hostname or "homelab";
  timezone = cfg.timezone or "UTC";
  locale = cfg.locale or "en_US.UTF-8";
  sshPort = cfg.ssh_port or 22;
  allowedSshKeys = cfg.allowed_ssh_keys or [ ];
  rootSshKey = cfg.root_ssh_key or "";
  homelabPasswordHash = cfg.homelab_password_hash or "";
  flakeTarget = sysCfg.flake_target or "yolab";
  repoPath = sysCfg.repo_path or "/opt/yolab";
  inherit tunnelCfg tunnelEnabled wgSubnet clientUi localApiEnv
          nodeAgentEnv swarmCfg swarmEnabled nodeCfg;
}
