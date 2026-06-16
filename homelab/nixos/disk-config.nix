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
            size = "100%FREE";
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
              mountOptions = ["defaults"];
            };
          };
        };
      };
    };
  };
}
