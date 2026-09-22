{
  modulesPath,
  yolabConfigPath,
  ...
}: let
  homelabConfig = builtins.fromTOML (builtins.readFile yolabConfigPath);
  bootMode = homelabConfig.homelab.boot_mode or "uefi";
in {
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./common.nix
  ];

  boot.loader.grub.enable = true;
  boot.loader.grub.efiSupport = bootMode != "bios";
  boot.loader.grub.device =
    if bootMode == "bios"
    then ""
    else "nodev";
  boot.loader.efi.canTouchEfiVariables = bootMode != "bios";

  networking.networkmanager.enable = true;
  users.users.homelab.extraGroups = ["networkmanager"];

  system.stateVersion = "24.05";
}
