{
  pkgs,
  lib,
  x86,
  outputs,
  ...
}: let
  inherit (pkgs.stdenv.hostPlatform) system;
  prefixed = kind: lib.mapAttrs' (name: lib.nameValuePair "${kind}-${name}");
  onThisSystem = lib.filterAttrs (_: os: os.pkgs.stdenv.hostPlatform.system == system) outputs.nixosConfigurations;
  appRuns = name: app:
    pkgs.runCommand "app-${name}" {} ''
      test -x ${app.program}
      touch $out
    '';
in
  prefixed "package" (removeAttrs (outputs.packages.${system} or {}) (lib.optional (!x86) "android-apk"))
  // prefixed "dev-shell" (outputs.devShells.${system} or {})
  // prefixed "app" (lib.mapAttrs appRuns (outputs.apps.${system} or {}))
  // prefixed "nixos" (lib.mapAttrs (_: os: os.config.system.build.toplevel) onThisSystem)
  // lib.optionalAttrs (outputs ? formatter.${system}) {formatter = outputs.formatter.${system};}
