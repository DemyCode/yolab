{
  modulesPath,
  lib,
  ...
}: let
  configPath = ../ignored/config.toml;
  homelabConfig =
    if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else {};
  bootMode = homelabConfig.homelab.boot_mode or "uefi";
in {
  imports =
    [
      (modulesPath + "/installer/scan/not-detected.nix")
      (modulesPath + "/profiles/qemu-guest.nix")
      ./common.nix
    ]
    ++ lib.optional (builtins.pathExists ../ignored/hardware-configuration.nix) ../ignored/hardware-configuration.nix;

  boot.loader = if bootMode == "bios" then {
    grub.enable = true;
    grub.device = homelabConfig.disk.device or "nodev";
  } else {
    systemd-boot.enable = true;
    efi.canTouchEfiVariables = true;
  };

  networking.networkmanager.enable = true;
  users.users.homelab.extraGroups = ["networkmanager"];

  system.stateVersion = "24.05";
}
