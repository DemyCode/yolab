{ ... }:
{
  imports = [ ./common.nix ];

  wsl.enable = true;
  wsl.defaultUser = "homelab";

  # Override the defaults defined in common.nix's options.yolab.*
  yolab.platform = "wsl";
  yolab.flakeTarget = "yolab-wsl";
  yolab.repoPath = "/etc/nixos";

  system.stateVersion = "24.05";
}
