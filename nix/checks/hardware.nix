{
  lib,
  x86,
  nixosSystems,
  failOn,
  ...
}: let
  ci = nixosSystems.yolab-ci;
  plain = ci.config;
  reportOf = name: hardware: builtins.toFile "facter-${name}.json" (builtins.toJSON {inherit hardware;});
  withReport = name: hardware: (ci.extendModules {specialArgs.yolabFacterPath = reportOf name hardware;}).config;
  evaluated = config: builtins.unsafeDiscardStringContext config.system.build.toplevel.drvPath;

  card = vendor: device: drivers: {
    vendor = {
      hex = vendor;
      value = lib.fromHexString vendor;
    };
    device = {
      hex = device;
      value = lib.fromHexString device;
    };
    driver_modules = drivers;
  };
  rtx3060 = card "10de" "2504" ["nouveau"];
  gtx1080 = card "10de" "1b80" ["nouveau"];
  gtx680 = card "10de" "1180" ["nouveau"];
  gtx580 = card "10de" "1080" ["nouveau"];
  rx7900 = card "1002" "744c" ["amdgpu"];
  hd7970 = card "1002" "6798" ["radeon"];
  iris = card "8086" "a7a0" ["i915"];

  matches = want: got:
    lib.all (
      k:
        if builtins.isAttrs want.${k}
        then matches want.${k} got.${k}
        else want.${k} == got.${k}
    ) (builtins.attrNames want);
in
  {
    a-machine-builds-for-the-processor-it-has = let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      pick = config: facter: import ../machine-system.nix {inherit config facter systems;};
      cases = {
        "a machine that says nothing is a PC, as every machine before ARM was" = {
          got = pick {} {};
          want = "x86_64-linux";
        };
        "the installer's answer wins" = {
          got = pick {homelab.system = "aarch64-linux";} {system = "x86_64-linux";};
          want = "aarch64-linux";
        };
        "without one, the hardware report says what the processor is" = {
          got = pick {homelab = {};} {system = "aarch64-linux";};
          want = "aarch64-linux";
        };
      };
      unknown = builtins.tryEval (pick {homelab.system = "riscv64-linux";} {});
      aarch64 = nixosSystems.yolab-ci-aarch64;
    in
      failOn "a-machine-builds-for-the-processor-it-has" {
        problems =
          map (n: "machine-system: ${n}") (lib.filter (n: cases.${n}.got != cases.${n}.want) (builtins.attrNames cases))
          ++ lib.optional unknown.success "an unsupported processor was accepted instead of stopping the rebuild"
          ++ lib.optional (aarch64.pkgs.stdenv.hostPlatform.system != "aarch64-linux") "the aarch64 CI system is not built for aarch64"
          ++ lib.optional (aarch64._module.specialArgs.localApiEnv.system != "aarch64-linux") "the aarch64 system would run a local-api built for another processor";
      };
  }
  // lib.optionalAttrs x86 {
    gpu-detection-picks-the-driver-each-card-can-run = let
      detect = import ../../homelab/nixos/gpu/detect.nix lib;
      on = cards: detect {hardware.graphics_card = cards;};
      cases = {
        "no report enables nothing" = {
          got = detect {};
          want = {
            any = false;
            nvidia.present = false;
            amd = false;
            intel = false;
          };
        };
        "a Turing-or-newer card runs the current driver with the open module" = {
          got = on [rtx3060];
          want = {
            any = true;
            nvidia = {
              present = true;
              branch = "stable";
              open = true;
            };
          };
        };
        "a Pascal card runs the 580 branch, the last that supports it, with the closed module" = {
          got = on [gtx1080];
          want.nvidia = {
            present = true;
            branch = "legacy_580";
            open = false;
          };
        };
        "one old card beside a new one pulls the whole machine to the branch both run on" = {
          got = on [
            gtx1080
            rtx3060
          ];
          want.nvidia = {
            branch = "legacy_580";
            open = false;
          };
        };
        "a Kepler card runs the 470 branch, the last NVIDIA driver for it, closed" = {
          got = on [gtx680];
          want = {
            any = true;
            nvidia = {
              present = true;
              unsupported = false;
              branch = "legacy_470";
              open = false;
            };
          };
        };
        "a Kepler card beside a Pascal one pulls the machine to 470, which drives both" = {
          got = on [
            gtx1080
            gtx680
          ];
          want.nvidia.branch = "legacy_470";
        };
        "a Fermi card has no NVIDIA driver that builds, so nouveau keeps it" = {
          got = on [gtx580];
          want = {
            any = false;
            nvidia = {
              present = false;
              nouveauOnly = true;
              unsupported = true;
              branch = null;
            };
          };
        };
        "an old Radeon still on the radeon driver is found as AMD" = {
          got = on [hd7970];
          want = {
            any = true;
            amd = true;
          };
        };
        "an AMD card is found by vendor" = {
          got = on [rx7900];
          want = {
            any = true;
            amd = true;
            nvidia.present = false;
          };
        };
        "an Intel iGPU is found by vendor" = {
          got = on [iris];
          want = {
            any = true;
            intel = true;
          };
        };
        "a card without vendor ids is still found by its kernel driver" = {
          got = on [{driver_modules = ["nouveau"];}];
          want.nvidia = {
            present = true;
            branch = "stable";
          };
        };
        "a laptop with an Intel iGPU and an NVIDIA dGPU drives both" = {
          got = on [
            iris
            rtx3060
          ];
          want = {
            intel = true;
            nvidia.present = true;
          };
        };
      };
      failures = lib.filter (name: !(matches cases.${name}.want cases.${name}.got)) (
        builtins.attrNames cases
      );
      withNvidia = withReport "nvidia" {graphics_card = [rtx3060];};
      withKepler = withReport "kepler" {graphics_card = [gtx680];};
      withIntel = withReport "intel" {graphics_card = [iris];};
      withAmd = withReport "amd" {graphics_card = [hd7970];};
      extraNames = config: map lib.getName config.hardware.graphics.extraPackages;
    in
      failOn "gpu-detection-picks-the-driver-each-card-can-run" {
        problems =
          map (n: "detect: ${n}") failures
          ++ lib.optional plain.hardware.nvidia-container-toolkit.enable "a machine without a report turned the NVIDIA container toolkit on"
          ++ lib.optional (!plain.hardware.uinput.enable) "game streaming needs /dev/uinput on every machine"
          ++ lib.optional (!withNvidia.hardware.nvidia-container-toolkit.enable) "an NVIDIA report did not turn on the CDI spec the device plugin reads"
          ++ lib.optional (!(builtins.elem "nvidia" withNvidia.services.xserver.videoDrivers)) "an NVIDIA report did not load the nvidia driver"
          ++ lib.optional (!(lib.hasPrefix "470." withKepler.hardware.nvidia.package.version)) "a Kepler report did not build NVIDIA's 470 driver"
          ++ lib.optional (withKepler.systemd.services.nvidia-container-toolkit-cdi-generator.unitConfig.ConditionPathExists or null != "/proc/driver/nvidia/version") "the CDI generator runs before NVIDIA's driver is loaded, so switching from nouveau fails the update"
          ++ lib.optional (withKepler.systemd.services.nvidia-persistenced.unitConfig.ConditionPathExists or null != "/proc/driver/nvidia/version") "nvidia-persistenced runs before NVIDIA's driver is loaded, so switching from nouveau fails the update"
          ++ lib.optional (!(builtins.elem "intel-media-driver" (extraNames withIntel))) "an Intel report did not add the VA-API driver for Broadwell and newer"
          ++ lib.optional (!(builtins.elem "intel-vaapi-driver" (extraNames withIntel))) "an Intel report did not add the VA-API driver for chips older than Broadwell"
          ++ lib.optional (!(builtins.elem "amdgpu.si_support=1" withAmd.boot.kernelParams)) "an AMD report left GCN 1 cards on the radeon driver, without Vulkan"
          ++ lib.optional (!(builtins.elem "amdgpu.cik_support=1" withAmd.boot.kernelParams)) "an AMD report left GCN 2 cards on the radeon driver, without Vulkan"
          ++ lib.optional (builtins.elem "amdgpu.si_support=1" plain.boot.kernelParams) "a machine without an AMD card got AMD kernel options";
        needs = [
          (evaluated withNvidia)
          (evaluated withKepler)
          (evaluated withIntel)
          (evaluated withAmd)
          "${withNvidia.hardware.nvidia.package}"
          "${withKepler.hardware.nvidia.package}"
        ];
      };

    hardware-report-picks-cpu-and-disk-modules = let
      intel = withReport "intel-cpu" {cpu = [{vendor_name = "GenuineIntel";}];};
      amd = withReport "amd-cpu" {cpu = [{vendor_name = "AuthenticAMD";}];};
    in
      failOn "hardware-report-picks-cpu-and-disk-modules" {
        problems =
          lib.optional (!intel.hardware.cpu.intel.updateMicrocode) "an Intel CPU did not get its microcode updates"
          ++ lib.optional intel.hardware.cpu.amd.updateMicrocode "an Intel CPU got AMD microcode"
          ++ lib.optional (!amd.hardware.cpu.amd.updateMicrocode) "an AMD CPU did not get its microcode updates"
          ++ lib.optional (!(builtins.elem "amd_pstate=active" amd.boot.kernelParams)) "an AMD CPU did not get the amd-pstate frequency driver"
          ++ lib.optional (builtins.elem "amd_pstate=active" intel.boot.kernelParams) "an Intel CPU got AMD kernel options"
          ++ lib.optional (!intel.hardware.enableRedistributableFirmware) "a machine with a report did not get the firmware its microcode ships in"
          ++ lib.optional (!intel.services.fstrim.enable) "a machine with a report does not TRIM its SSDs"
          ++ lib.optional plain.hardware.cpu.intel.updateMicrocode "a machine without a report guessed its CPU";
        needs = [
          (evaluated intel)
          (evaluated amd)
        ];
      };
  }
