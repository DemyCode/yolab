{
  pkgs,
  treefmtEval,
  rust,
  nixosSystems,
  androidApk,
}: let
  inherit (pkgs) lib;

  treeSrc =
    pkgs.nix-gitignore.gitignoreRecursiveSource [
      ".git/"
      "target/"
      "node_modules/"
      "result"
      "result-*"
    ]
    ../..;

  failOn = name: {
    problems,
    explain ? "",
    needs ? [],
  }:
    pkgs.runCommand name {} ''
      ${lib.optionalString (problems != []) ''
        ${lib.optionalString (explain != "") "echo ${lib.escapeShellArg explain} >&2\necho >&2\n"}
        ${lib.concatMapStrings (p: "echo ${lib.escapeShellArg p} >&2\n") problems}
        exit 1
      ''}
      ${lib.concatMapStrings (n: "echo ${lib.escapeShellArg n} > /dev/null\n") needs}
      touch $out
    '';

  ctx = {
    inherit pkgs lib treeSrc nixosSystems failOn treefmtEval androidApk;
    inherit (rust) crates;
    x86 = pkgs.stdenv.hostPlatform.system == "x86_64-linux";
    cfg = nixosSystems.yolab-ci.config;
    builds = import ../../homelab/builds.nix {inherit pkgs rust;};
  };

  groups = map (f: import f ctx) [
    ./builds.nix
    ./scripts.nix
    ./lint.nix
    ./source.nix
    ./units.nix
    ./storage.nix
    ./security.nix
    ./hardware.nix
  ];

  names = lib.concatMap builtins.attrNames groups;
  twice = lib.filter (n: lib.count (m: m == n) names > 1) (lib.unique names);
in
  if twice != []
  then throw "nix/checks defines these checks in more than one file: ${toString twice}"
  else lib.mergeAttrsList groups
