{
  lib,
  yolabFacterPath ? null,
  nixosHardware ? null,
  ...
}: let
  report =
    if yolabFacterPath == null
    then {}
    else lib.importJSON yolabFacterPath;
  machine = import ./detect.nix lib report;
in {
  imports = lib.optionals (nixosHardware != null && machine.known) (
    [nixosHardware.nixosModules.common-pc-ssd]
    ++ lib.optional machine.intel nixosHardware.nixosModules.common-cpu-intel-cpu-only
    ++ lib.optional machine.amd nixosHardware.nixosModules.common-cpu-amd-pstate
  );

  config = lib.mkIf machine.known {
    hardware.enableRedistributableFirmware = true;
  };
}
