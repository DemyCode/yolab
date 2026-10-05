{
  config,
  lib,
  pkgs,
  yolabFacterPath ? null,
  ...
}: let
  report =
    if yolabFacterPath == null
    then {}
    else lib.importJSON yolabFacterPath;
  gpus = import ./detect.nix lib report;

  reportFile = "${config.yolab.machineDir}/facter.json";
  writeReport = pkgs.writeShellApplication {
    name = "yolab-hardware-report";
    runtimeInputs = [pkgs.nixos-facter pkgs.coreutils];
    text = ''
      tmp="$(mktemp ${lib.escapeShellArg reportFile}.XXXXXX)"
      trap 'rm -f "$tmp"' EXIT
      nixos-facter -o "$tmp"
      chmod 0600 "$tmp"
      mv "$tmp" ${lib.escapeShellArg reportFile}
      trap - EXIT
    '';
  };
in {
  config = lib.mkMerge [
    {
      hardware.uinput.enable = true;
      boot.kernelModules = ["uhid"];

      environment.systemPackages = [pkgs.nixos-facter];

      systemd.services.yolab-hardware-report = {
        description = "Record this machine's hardware so the next rebuild can drive its GPU";
        wantedBy = ["multi-user.target"];
        after = ["local-fs.target"];
        serviceConfig = {
          Type = "oneshot";
          ExecStart = lib.getExe writeReport;
        };
      };
    }

    (lib.mkIf gpus.any {
      hardware.graphics.enable = true;
      hardware.enableRedistributableFirmware = true;
    })

    (lib.mkIf gpus.amd {
      boot.initrd.kernelModules = ["amdgpu"];
    })

    (lib.mkIf gpus.nvidia.present {
      nixpkgs.config.allowUnfreePredicate = pkg: lib.hasPrefix "nvidia" (lib.getName pkg);

      services.xserver.videoDrivers = ["nvidia"];

      hardware.nvidia = {
        inherit (gpus.nvidia) branch open;
        modesetting.enable = true;
        nvidiaPersistenced = true;
        nvidiaSettings = false;
      };

      hardware.nvidia-container-toolkit.enable = true;
    })

    (lib.mkIf gpus.nvidia.unsupported {
      warnings = [
        "This machine has an NVIDIA GPU older than Maxwell (GTX 7xx or earlier). No current driver supports it, so it is not used."
      ];
    })
  ];
}
