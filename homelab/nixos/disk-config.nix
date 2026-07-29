{lib, ...}: let
  configPath = ../ignored/config.toml;
  homelabConfig =
    if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else throw "config.toml not found! Please create it from config.toml.example";

  diskConfig = homelabConfig.disk or (throw "[disk] section missing in config.toml");
  diskDevice = diskConfig.device or (throw "[disk] device is required in config.toml");
  espSize = diskConfig.esp_size or (throw "[disk] esp_size is required in config.toml");
  bootMode = homelabConfig.homelab.boot_mode or "uefi";
  # Size of the OS root LV; the rest of the disk becomes the Ceph OSD LV.
  # 40 GiB comfortably holds NixOS + a few generations + k3s + container images
  # for a multi-app node. Configurable per-disk; adjustable later via lvextend.
  systemSize = diskConfig.system_size or "40G";
in {
  disko.devices = {
    disk.disk1 = {
      device = lib.mkDefault diskDevice;
      type = "disk";
      content = {
        type = "gpt";
        partitions =
          (if bootMode == "bios" then {
            # 1 MB BIOS boot partition — GRUB embeds its core.img here on GPT disks.
            # No content block: GRUB writes directly to this partition.
            bios = {
              name = "BIOS";
              size = "1M";
              type = "EF02"; # for GRUB MBR on GPT
            };
          } else {
            esp = {
              name = "ESP";
              size = espSize;
              type = "EF00";
              content = {
                type = "filesystem";
                format = "vfat";
                mountpoint = "/boot";
              };
            };
          })
          // {
            root = {
              name = "root";
              size = "100%";
              content = {
                type = "lvm_pv";
                vg = "pool";
              };
            };
          };
      };
    };
    lvm_vg = {
      pool = {
        type = "lvm_vg";
        lvs = {
          root = {
            size = systemSize;
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
              mountOptions = ["defaults"];
            };
          };
          # Raw LV consumed directly by Ceph/Rook (BlueStore on the block device,
          # no filesystem). Takes the whole remainder of the disk. Unlike the old
          # loop-file OSD this has no page-cache/coherence hacks and no ENOSPC
          # coupling with the OS root, and it activates automatically on boot.
          ceph = {
            size = "100%FREE";
          };
        };
      };
    };
  };
}
