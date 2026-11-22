{
  pkgs,
  lib,
  x86,
  builds,
  crates,
  nixosSystems,
  androidApk,
  ...
}: let
  toplevel = name: nixosSystems.${name}.config.system.build.toplevel;
in
  {
    client-ui = builds.clientUi;
    client-ui-tests = builds.clientUiTests;
    client-ui-lint = builds.clientUiLint;

    local-api-tests = crates.local-api.tests;
    installer-tests = crates.installer.tests;
    desktop-client-tests = crates.desktop-client.tests;

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
