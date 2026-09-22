{
  lib,
  yolabConfigPath,
  ...
}: let
  homelabConfig = builtins.fromTOML (builtins.readFile yolabConfigPath);

  diskConfig = homelabConfig.disk or (throw "[disk] section missing in config.toml");
  diskDevice = diskConfig.device or (throw "[disk] device is required in config.toml");
  espSize = diskConfig.esp_size or (throw "[disk] esp_size is required in config.toml");
  bootMode = homelabConfig.homelab.boot_mode or "uefi";
  systemSize = diskConfig.system_size or "60G";
in {
  disko.devices = {
    disk.disk1 = {
      device = lib.mkDefault diskDevice;
      type = "disk";
      content = {
        type = "gpt";
        partitions =
          (
            if bootMode == "bios"
            then {
              bios = {
                name = "BIOS";
                size = "1M";
                type = "EF02";
              };
            }
            else {
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
            }
          )
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
          ceph = {
            size = "100%FREE";
          };
        };
      };
    };
  };
}
