{
  config,
  lib,
  pkgs,
  yolabFacterPath ? null,
  nixosHardware ? null,
  ...
}: let
  report =
    if yolabFacterPath == null
    then {}
    else lib.importJSON yolabFacterPath;
  gpus = import ./detect.nix lib report;

  aur470 = pkgs.fetchgit {
    url = "https://aur.archlinux.org/nvidia-470xx-utils.git";
    rev = "af0b7617132e32dd39174779aa8ced2a726afc51";
    hash = "sha256-2PCK42OH0oxRlG5R9ptXDTOWMsMD7wdBZJIRsZC46AI=";
  };
  legacy470 = config.boot.kernelPackages.nvidiaPackages.legacy_470.overrideAttrs (old: {
    patches =
      old.patches
      ++ map (p: "${aur470}/${p}") [
        "nvidia-470xx-fix-linux-7.2-part1.patch"
        "nvidia-470xx-fix-linux-7.2-part2.patch"
        "nvidia-470xx-fix-linux-7.2-part3.patch"
        "nvidia-470xx-fix-linux-7.3.patch"
      ];
  });

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
  imports = lib.optionals (nixosHardware != null) (
    lib.optional gpus.intel nixosHardware.nixosModules.common-gpu-intel
    ++ lib.optionals gpus.amd [
      nixosHardware.nixosModules.common-gpu-amd-southern-islands
      nixosHardware.nixosModules.common-gpu-amd-sea-islands
    ]
  );

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
      nixpkgs.config.nvidia.acceptLicense = true;

      services.xserver.videoDrivers = ["nvidia"];

      hardware.nvidia = {
        inherit (gpus.nvidia) branch open;
        modesetting.enable = true;
        nvidiaPersistenced = true;
        nvidiaSettings = false;
        package = lib.mkIf (gpus.nvidia.branch == "legacy_470") legacy470;
      };

      hardware.nvidia-container-toolkit.enable = true;

      systemd.services.nvidia-container-toolkit-cdi-generator.unitConfig.ConditionPathExists = "/proc/driver/nvidia/version";
      systemd.services.nvidia-persistenced.unitConfig.ConditionPathExists = "/proc/driver/nvidia/version";
    })

    (lib.mkIf gpus.nvidia.unsupported {
      warnings = [
        "This machine has an NVIDIA GPU older than Kepler (GTX 5xx or earlier). NVIDIA's last driver for it no longer builds, so it only gets the open nouveau driver: a display, no AI or video acceleration."
      ];
    })
  ];
}
