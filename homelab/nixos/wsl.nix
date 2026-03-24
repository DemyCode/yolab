{ ... }:
{
  imports = [ ./common.nix ];

  wsl.enable = true;
  wsl.defaultUser = "homelab";

  yolab.platform = "wsl";
  yolab.flakeTarget = "yolab-wsl";
  yolab.repoPath = "/etc/nixos";

  system.stateVersion = "24.05";
}
