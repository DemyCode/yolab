{ modulesPath, lib, ... }:
{
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./common.nix
  ] ++ lib.optional (builtins.pathExists ../ignored/hardware-configuration.nix) ../ignored/hardware-configuration.nix;

  yolab.platform = "nixos";
  yolab.flakeTarget = "yolab";
  yolab.repoPath = "/etc/nixos";

  boot.loader.systemd-boot.enable = true;

  networking.networkmanager.enable = true;
  users.users.homelab.extraGroups = [ "networkmanager" ];

  system.stateVersion = "24.05";
}
