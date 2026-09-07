# Building shells/desktop as an Android APK.
#
# Android is the one target that genuinely cross-compiles: the toolchain runs on
# Linux and emits for `*-linux-android`, so nothing here needs a phone or a Mac.
# What it does need is Gradle, and Gradle is the entire difficulty.
#
# Gradle resolves its dependencies over the network at build time. A nix sandbox
# has no network, so the dependency download happens once in a fixed-output
# derivation — the only kind allowed to reach the internet, in exchange for
# declaring a hash of everything it produces up front. The real build then runs
# offline against that cache.
#
# The consequence: `outputHash` below cannot be known before the first build.
# It is left as `lib.fakeHash`; nix prints the correct value in the mismatch
# error and it goes in the file. That is the intended workflow for a
# fixed-output derivation, not a mistake — but it does mean the first
# `nix build .#android-apk` after any dependency change is expected to fail,
# and to say exactly what to paste in.
{
  pkgs,
  rust,
  lib ? pkgs.lib,
}:
let
  # The Android SDK is unfree — Google's licence — and the repo's main `pkgs` is
  # plain `nixpkgs.legacyPackages`, with no allowUnfree. Evaluating it there
  # fails with a licence refusal naming a package nobody asked for by name,
  # which reads like a nixpkgs bug rather than a policy.
  #
  # A second instance scoped to this file, rather than allowUnfree on the whole
  # flake: every other package in this repo is free and should stay that way, so
  # the exception is confined to the one build that needs it. `accept_license`
  # is separately required, and its absence is a different and equally opaque
  # error.
  androidPkgs = import pkgs.path {
    inherit (pkgs) system;
    config = {
      allowUnfree = true;
      android_sdk.accept_license = true;
    };
  };

  # Pinned, not "latest". An SDK that moves under you turns a reproducible build
  # into one that works today; and the NDK version in particular has to match
  # what Tauri's Gradle plugin expects, or the failure is a linker error deep in
  # a Rust build rather than anything naming a version.
  androidSdk = androidPkgs.androidenv.composeAndroidPackages {
    # 35 because that is what Tauri's generated Gradle project asks for:
    #
    #   Could not determine the dependencies of task ':app:minifyUniversalReleaseWithR8'.
    #   > Failed to find Build Tools revision 35.0.0
    #
    # 34 is kept alongside it because a missing SDK component does not fail
    # cleanly — the SDK manager goes looking for it on dl.google.com, and with
    # no network that produces pages of UnknownHostException stack traces that
    # bury the one line naming the actual missing revision. Carrying both costs
    # download size and removes a whole class of misleading failure.
    platformVersions = [
      "34"
      "35"
    ];
    buildToolsVersions = [
      "34.0.0"
      "35.0.0"
    ];
    includeNDK = true;
    ndkVersions = [ "26.1.10909125" ];
    cmakeVersions = [ "3.22.1" ];
    includeEmulator = false;
    includeSystemImages = false;
  };

  sdkRoot = "${androidSdk.androidsdk}/libexec/android-sdk";
  ndkRoot = "${sdkRoot}/ndk-bundle";

  # The desktop crate's source, reused verbatim so the APK is built from exactly
  # what `nix build .#desktop-client` builds.
  src = rust.crates.desktop-client.args.src;

  commonEnv = {
    ANDROID_HOME = sdkRoot;
    ANDROID_SDK_ROOT = sdkRoot;
    ANDROID_NDK_ROOT = ndkRoot;
    NDK_HOME = ndkRoot;
  };

  # Rust's dependencies, vendored into the store.
  #
  # Two ecosystems download things here, and caching only one of them is a trap
  # worth naming: step 1 caches Gradle's jars, but `tauri android build` also
  # runs `cargo build`, and cargo wants the crates.io index. With no network in
  # step 2 that fails with
  #
  #   Updating crates.io index
  #   Could not resolve host: index.crates.io
  #
  # which looks like the Gradle cache failing, and is not related to it at all.
  #
  # craneLib.vendorCargoDeps is the same mechanism the rest of this repo's Rust
  # builds already use, so the crates come from the same source and the same
  # Cargo.lock as `nix build .#desktop-client`.
  cargoVendorDir = rust.craneLib.vendorCargoDeps { inherit src; };

  # Points cargo at the vendored copy instead of the network. Needed in both
  # derivations: step 1 could reach crates.io, but using the vendored source
  # there too means the two steps resolve identical dependencies rather than
  # merely similar ones.
  cargoOffline = ''
    export CARGO_HOME=$TMPDIR/cargo
    mkdir -p "$CARGO_HOME"
    cp ${cargoVendorDir}/config.toml "$CARGO_HOME/config.toml"
  '';

  # Tauri assembles the APK by running `gen/android/gradlew`, the Gradle wrapper
  # script — and `tauri android init` does not produce one, so the build dies
  # with "`gradlew` not found. Make sure you have the Android SDK installed".
  # The message is misleading: the SDK is fine, the wrapper simply is not there.
  #
  # The fix is a shim rather than `gradle wrapper`, because the wrapper's whole
  # purpose is to fetch a pinned Gradle distribution at run time — exactly the
  # network dependency this file exists to eliminate, and pointless when
  # nixpkgs already pins one. Two lines that hand the call to the Gradle on
  # PATH, which is the version nix chose.
  gradlewShim = ''
    printf '#!/bin/sh\nexec gradle "$@"\n' > gen/android/gradlew
    chmod +x gen/android/gradlew
  '';

  nativeBuildInputs = [
    rust.rustToolchain
    pkgs.cargo-tauri
    androidSdk.androidsdk
    pkgs.jdk17
    pkgs.gradle
    pkgs.pkg-config
    pkgs.nodejs
  ];

  # ── Step 1: the dependency cache ───────────────────────────────────────────
  #
  # Generates the Gradle project, runs a full build WITH network, and keeps only
  # the dependencies it downloaded. This is the one derivation allowed online.
  #
  # A full build rather than a cheaper dependency-resolution step, because the
  # cheaper step does not work — see the buildPhase.
  gradleDeps = pkgs.stdenv.mkDerivation (
    {
      pname = "yolab-android-gradle-deps";
      version = "0.1.0";
      inherit src nativeBuildInputs;

      buildPhase = ''
        runHook preBuild
        export HOME=$TMPDIR
        # Set HERE, not as a derivation attribute: nix does not expand $TMPDIR in
        # an env attribute, so it would arrive as the literal string and mkdir
        # would cheerfully create a directory named $TMPDIR.
        export GRADLE_USER_HOME=$TMPDIR/gradle
        ${cargoOffline}
        mkdir -p "$GRADLE_USER_HOME"

        # Generates gen/android. Not committed to the repo: it is a template
        # render, and a checked-in copy silently goes stale against the CLI that
        # produced it.
        cargo tauri android init --ci
        ${gradlewShim}

        # The REAL build, not `gradle dependencies`, and not run from
        # gen/android by hand. Tauri writes gen/android/tauri.settings.gradle as
        # part of driving the build; settings.gradle line 3 applies that file, so
        # invoking gradle directly after `init` fails with
        #
        #   Could not read script '.../tauri.settings.gradle' as it does not exist
        #
        # Only the CLI knows how to get the project into a buildable state, so
        # the cheapest correct thing is to let it do the whole build here — with
        # network — and keep nothing but the downloaded dependencies.
        #
        # `|| true` because this derivation's product is the cache, not the APK.
        # A build that fails late still leaves the dependencies it resolved.
        cargo tauri android build --apk || true
        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        mkdir -p $out

        # ONLY the downloaded artifacts. Copying `caches` wholesale is what made
        # this unreproducible: it also holds lock files, a journal and
        # gc.properties, all of which differ between two identical runs — so the
        # output hash changed every build and could never be pinned. Observed
        # directly: two runs of the same input produced H/23kSjD… and sHv54rq6….
        #
        # modules-2/files-2.1 is the jars themselves, keyed by their own
        # checksums, which is the one part that IS stable.
        cp -r "$GRADLE_USER_HOME"/caches/modules-2 $out/ 2>/dev/null || true

        # Belt and braces: these appear inside modules-2 as well.
        find $out -name '*.lock' -delete
        find $out -name 'gc.properties' -delete
        find $out -type d -empty -delete

        if [ -z "$(ls -A $out)" ]; then
          echo "no Gradle dependencies were cached — the build resolved nothing" >&2
          exit 1
        fi
        runHook postInstall
      '';

      # Fixed-output: the three attributes below are what buy network access.
      outputHashMode = "recursive";
      outputHashAlgo = "sha256";
      # Pinned from a build's mismatch error; see this file's header.
      outputHash = "sha256-fJc8lCr2qFIAZB+i303t5MIOfzFOkQZDJKPeBTE/PEk=";
    }
    // commonEnv
  );

  # ── Step 2: the APK itself ─────────────────────────────────────────────────
in
pkgs.stdenv.mkDerivation (
  {
    pname = "yolab-android-apk";
    version = "0.1.0";
    inherit src nativeBuildInputs;

    buildPhase = ''
      runHook preBuild
      export HOME=$TMPDIR
      # Set HERE, not as a derivation attribute: nix does not expand $TMPDIR in
      # an env attribute, so it would arrive as the literal string and mkdir
      # would cheerfully create a directory named $TMPDIR.
      export GRADLE_USER_HOME=$TMPDIR/gradle
      ${cargoOffline}
      # Restored under caches/, which is where step 1 took it from. Step 1
      # stores only `modules-2` — see its installPhase for why the rest cannot
      # be kept — so the parent directory is recreated here.
      mkdir -p "$GRADLE_USER_HOME/caches"
      cp -r ${gradleDeps}/modules-2 "$GRADLE_USER_HOME/caches/"
      chmod -R u+w "$GRADLE_USER_HOME"

      cargo tauri android init --ci
      ${gradlewShim}

      # No network in the sandbox, so a dependency step 1 failed to cache fails
      # here — which is the intended signal, not a surprise.
      cargo tauri android build --apk

      runHook postBuild
    '';

    installPhase = ''
      runHook preInstall
      mkdir -p $out
      find gen/android -name '*.apk' -exec cp {} $out/ \;
      # Fail loudly rather than producing an empty output that looks like a
      # success until someone tries to install it.
      if [ -z "$(ls -A $out)" ]; then
        echo "no APK was produced — the Gradle assemble step did not run" >&2
        exit 1
      fi
      runHook postInstall
    '';

    meta = {
      description = "YoLab as an Android APK (unsigned, for sideloading)";
      platforms = [ "x86_64-linux" ];
    };
  }
  // commonEnv
)
