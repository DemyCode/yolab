{
  pkgs,
  treefmtEval,
  rust,
  nixosSystems,
  androidApk,
  vm,
  outputs,
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
    inherit pkgs lib treeSrc nixosSystems failOn treefmtEval androidApk vm outputs;
    inherit (rust) crates;
    x86 = pkgs.stdenv.hostPlatform.system == "x86_64-linux";
    cfg = nixosSystems.yolab-ci.config;
    builds = import ../../homelab/builds.nix {inherit pkgs rust;};
  };

  load = map (f: import f ctx);
  short = load [
    ./builds.nix
    ./scripts.nix
    ./lint.nix
    ./source.nix
    ./units.nix
    ./storage.nix
    ./security.nix
    ./hardware.nix
    ./outputs.nix
  ];
  extra = load [
    ./mutants.nix
    ./vm.nix
  ];

  names = lib.concatMap builtins.attrNames (short ++ extra);
  twice = lib.filter (n: lib.count (m: m == n) names > 1) (lib.unique names);
  long = lib.mergeAttrsList (short ++ extra);
in
  if twice != []
  then throw "nix/checks defines these checks in more than one file: ${toString twice}"
  else {
    short = lib.mergeAttrsList short;
    long =
      long
      // {
        all = pkgs.linkFarm "all-long-checks" (lib.mapAttrsToList (name: path: {inherit name path;}) long);
      };
  }
