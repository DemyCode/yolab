{
  pkgs,
  lib,
  treeSrc,
  treefmtEval,
  nixosSystems,
  failOn,
  ...
}: {
  formatting = treefmtEval.config.build.check treeSrc;

  nixos-warnings = failOn "nixos-warnings" {
    explain = "NixOS printed warnings while building these machines. Warnings fail the checks like errors:";
    problems = lib.concatLists (lib.mapAttrsToList
      (name: system: map (w: "${name}: ${w}") system.config.warnings)
      nixosSystems);
  };
  shellcheck =
    pkgs.runCommand "shellcheck"
    {
      nativeBuildInputs = [pkgs.shellcheck];
    }
    ''
      find ${treeSrc} -name '*.sh' -print0 | xargs -0 shellcheck -x
      touch $out
    '';

  hadolint =
    pkgs.runCommand "hadolint"
    {
      nativeBuildInputs = [pkgs.hadolint];
    }
    ''
      find ${treeSrc} -name 'Dockerfile' -print0 \
        | xargs -0 hadolint --ignore DL3018
      touch $out
    '';
  deadnix =
    pkgs.runCommand "deadnix"
    {
      nativeBuildInputs = [pkgs.deadnix];
    }
    ''
      deadnix --fail ${treeSrc}
      touch $out
    '';
}
