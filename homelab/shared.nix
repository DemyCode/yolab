{
  pkgs,
  inputs,
  ...
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
    tunnelCfg
    nodeCfg
    tunnelEnabled
    privateSubnet
    clientUi
    localApiEnv
    ;
}
