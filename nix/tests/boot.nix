# Single-node VM boot test: prove a yolab node boots, bootstraps Ceph, and
# brings up k3s. This is the harness skeleton for the two-node quorum/CSI/noout
# test — it validates the plumbing (specialArgs, disko neutralisation, boot)
# before the mesh and join paths are added.
{
  pkgs,
  inputs,
  rust,
  disko,
}: let
  bootConfigPath = ../../homelab/tests/boot-config.toml;
  baseModules = [
    disko.nixosModules.disko
    ../../homelab/nixos/configuration.nix
    ../../homelab/nixos/disk-config.nix
  ];
  vmModule = {lib, ...}: {
    _module.args = {
      inherit inputs rust;
      yolabConfigPath = bootConfigPath;
    };
    imports = baseModules;

    # The VM boots from the test harness's own root image, not the LVM layout a
    # real install uses, so the install-time disk layout is neutralised and GRUB
    # is pointed at the VM's virtual disk.
    disko.devices = lib.mkForce {};
    boot.loader.grub.enable = lib.mkForce true;
    boot.loader.grub.device = lib.mkForce "/dev/vda";
    boot.loader.grub.efiSupport = lib.mkForce false;
    virtualisation.memorySize = 2048;
  };
in
  pkgs.testers.nixosTest {
    name = "yolab-boot";
    nodes.node1 = vmModule;
    testScript = ''
      node1.wait_for_unit("multi-user.target", timeout=600)
      node1.succeed("systemctl is-active ceph-mon-yolab-n1.service")
      node1.succeed("systemctl is-active k3s.service")
    '';
  }
