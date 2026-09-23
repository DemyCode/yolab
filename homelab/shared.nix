{
  pkgs,
  yolabConfigPath,
  rust,
  ...
}: let
  homelabConfig = builtins.fromTOML (builtins.readFile yolabConfigPath);

  cfg = homelabConfig.homelab;
  tunnelCfg = homelabConfig.tunnel or {};
  nodeCfg = homelabConfig.node or {};

  privateSubnet = nodeCfg.sub_ipv6_private_subnet or "fd00:cafe::/112";

  builds = import ./builds.nix {inherit pkgs rust;};
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
    privateSubnet
    clientUi
    localApiEnv
    ;
}
