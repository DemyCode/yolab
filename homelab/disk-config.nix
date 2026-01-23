# Generic disk configuration for homelab
# Disk device is loaded from config.toml
{ lib, ... }:

let
  # Load machine-specific configuration from TOML
  configPath = ./config.toml;
  homelabConfig = if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else {};

  # Extract disk configuration
  diskConfig = homelabConfig.disk or {};
  diskDevice = diskConfig.device or "/dev/sda";
  espSize = diskConfig.esp_size or "500M";
  swapSize = diskConfig.swap_size or "8G";

in {
  disko.devices = {
    disk.disk1 = {
      device = lib.mkDefault diskDevice;
      type = "disk";
      content = {
        type = "gpt";
        partitions = {
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
          swap = {
            size = swapSize;
            content = {
              type = "swap";
            };
          };
          root = {
            size = "100%FREE";
            content = {
              type = "filesystem";
              format = "ext4";
              mountpoint = "/";
              mountOptions = [ "defaults" ];
            };
          };
        };
      };
    };
  };
}
