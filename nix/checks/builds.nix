{
  pkgs,
  lib,
  x86,
  builds,
  crates,
  nixosSystems,
  androidApk,
  failOn,
  ...
}: let
  toplevel = name: nixosSystems.${name}.config.system.build.toplevel;
  missingUntested =
    lib.concatLists (lib.mapAttrsToList (name: crate: map (f: "${name}: ${f}") crate.untestedMissing) crates)
    ++ map (f: "client-ui: ${f}") (lib.filter (f: !builtins.pathExists (../../homelab/client-ui + "/${f}")) (builtins.attrNames builds.clientUiUntested));
in
  {
    client-ui = builds.clientUi;
    client-ui-tests = builds.clientUiTests;
    client-ui-lint = builds.clientUiLint;
    client-ui-coverage = builds.clientUiCoverage;
    client-ui-mutants = builds.clientUiMutants;

    local-api-tests = crates.local-api.tests;
    installer-tests = crates.installer.tests;
    desktop-client-tests = crates.desktop-client.tests;

    local-api-coverage = crates.local-api.coverage;
    installer-coverage = crates.installer.coverage;
    desktop-client-coverage = crates.desktop-client.coverage;

    local-api-mutants = crates.local-api.mutants;
    installer-mutants = crates.installer.mutants;
    desktop-client-mutants = crates.desktop-client.mutants;

    untested-exceptions-name-real-files = failOn "untested-exceptions-name-real-files" {
      problems = missingUntested;
      explain = "nix/untested.nix and homelab/client-ui/untested.json may only name files that exist; coverage and mutation testing ignore a stale entry silently:";
    };

    clippy-local-api = crates.local-api.clippy;
    clippy-installer = crates.installer.clippy;
    clippy-desktop-client = crates.desktop-client.clippy;

    nixos-create = toplevel "yolab-ci";
    nixos-join = toplevel "yolab-ci-join";

    disko-create = nixosSystems.yolab-ci.config.system.build.diskoScript;
    disko-join = nixosSystems.yolab-ci-join.config.system.build.diskoScript;
  }
  // lib.optionalAttrs x86 {
    android-apk-is-signed-so-android-will-install-it =
      pkgs.runCommand "android-apk-is-signed-so-android-will-install-it"
      {nativeBuildInputs = [pkgs.jdk17];}
      ''
        test "$(ls ${androidApk}/*.apk)" = "${androidApk}/yolab.apk"
        ${androidApk.buildTools}/apksigner verify --print-certs ${androidApk}/yolab.apk
        touch $out
      '';
  }
