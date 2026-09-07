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
# The consequence: `gradleDepsHash` below cannot be known before the first build.
# Leave it as `lib.fakeHash`, build once, and nix prints the correct value in the
# mismatch error. That is the intended workflow for a fixed-output derivation,
# not a mistake — but it does mean the first `nix build .#android-apk` is
# expected to fail, and to say exactly what to paste in.
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
    platformVersions = [ "34" ];
    buildToolsVersions = [ "34.0.0" ];
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
  # Runs `tauri android init` (which generates the Gradle project) and then a
  # dependency-only Gradle invocation, and captures the resulting cache. This is
  # the one derivation permitted network access.
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
        mkdir -p "$GRADLE_USER_HOME"

        # Generates gen/android. Not committed to the repo: it is a template
        # render, and a checked-in copy silently goes stale against the CLI that
        # produced it.
        cargo tauri android init --ci

        cd gen/android
        # `dependencies` resolves the whole graph without compiling anything,
        # which is all this derivation exists to cache.
        gradle --no-daemon --console=plain dependencies || true
        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        mkdir -p $out
        cp -r "$GRADLE_USER_HOME"/caches $out/ || true
        runHook postInstall
      '';

      # Fixed-output: the three attributes below are what buy network access.
      outputHashMode = "recursive";
      outputHashAlgo = "sha256";
      # REPLACE ME after the first build — see this file's header.
      outputHash =  "sha256-H/23kSjDk9YbjrQhIcME+cn3q04O3ZbTGeRPyv/h4U8=";
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
      mkdir -p "$GRADLE_USER_HOME"
      cp -r ${gradleDeps}/caches "$GRADLE_USER_HOME"/
      chmod -R u+w "$GRADLE_USER_HOME"

      cargo tauri android init --ci
      # No network in the sandbox, so a dependency step 1 failed to cache
      # fails here — which is the intended signal, not a surprise.
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
