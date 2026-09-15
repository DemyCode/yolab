{
  modulesPath,
  yolabConfigPath,
  ...
}: let
  homelabConfig = builtins.fromTOML (builtins.readFile yolabConfigPath);
  bootMode = homelabConfig.homelab.boot_mode or "uefi";
in {
  # The machine's hardware-configuration.nix is added by flake.nix, from the
  # `yolab-machine` input.
  imports = [
    (modulesPath + "/installer/scan/not-detected.nix")
    (modulesPath + "/profiles/qemu-guest.nix")
    ./common.nix
  ];

  # GRUB works on both BIOS and UEFI.
  # On BIOS: disko auto-sets grub.devices from the EF02 partition — don't also set grub.device or it duplicates mirroredBoots.
  # On UEFI: device = "nodev" tells GRUB to install as an EFI binary rather than to an MBR.
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
