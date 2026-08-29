# yolabConfigPath is threaded in from flake.nix rather than hardcoded here.
#
# This file used to read ./ignored/config.toml directly and unconditionally,
# which made it the reason `nix flake check` could not run on a fresh clone:
# common.nix imports it for every NixOS config and darwin/configuration.nix for
# both Darwin ones, so a missing file failed the evaluation of every machine
# output. With the path as an argument the same modules can be evaluated
# against the committed CI stubs, and a node still gets its real config.
{
  pkgs,
  inputs,
  yolabConfigPath,
  ...
}: let
  homelabConfig = builtins.fromTOML (builtins.readFile yolabConfigPath);

  cfg = homelabConfig.homelab;
  tunnelCfg = homelabConfig.tunnel or {};
  nodeCfg = homelabConfig.node or {};

  # Whether the tunnel section is populated (installer has run pairing).
  tunnelEnabled = (tunnelCfg.sub_ipv6 or "") != "";

  # The /112 private subnet covering all nodes' cluster IPs.
  # Stored by the installer in config.toml so all nodes share the same value.
  # Falls back to a sensible ULA default if absent (e.g. dev/WSL).
  privateSubnet = nodeCfg.sub_ipv6_private_subnet or "fd00:cafe::/112";

  builds = import ./builds.nix {inherit pkgs inputs;};
  inherit (builds) clientUi localApiEnv;
in {
  inherit (cfg) hostname;
  inherit (cfg) timezone;
  inherit (cfg) locale;
  sshPort = cfg.ssh_port;
  allowedSshKeys = cfg.allowed_ssh_keys or [];
  rootSshKey = cfg.root_ssh_key or "";
  homelabPasswordHash = cfg.homelab_password_hash or "";

  inherit
    homelabConfig
    tunnelCfg
    nodeCfg
    tunnelEnabled
    privateSubnet
    clientUi
    localApiEnv
    ;
}
