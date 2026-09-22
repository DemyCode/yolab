{
  pkgs,
  rust,
  lib ? pkgs.lib,
}: let
  androidPkgs = import pkgs.path {
    inherit (pkgs) system;
    config = {
      allowUnfree = true;
      android_sdk.accept_license = true;
    };
  };

  androidSdk = androidPkgs.androidenv.composeAndroidPackages {
    platformVersions = [
      "34"
      "35"
      "36"
    ];
    buildToolsVersions = [
      "34.0.0"
      "35.0.0"
      "36.0.0"
    ];
    includeNDK = true;
    ndkVersions = ["26.1.10909125"];
    cmakeVersions = ["3.22.1"];
    includeEmulator = false;
    includeSystemImages = false;
  };

  sdkRoot = "${androidSdk.androidsdk}/libexec/android-sdk";
  ndkRoot = "${sdkRoot}/ndk-bundle";

  src = rust.crates.desktop-client.args.src;

  commonEnv = {
    ANDROID_HOME = sdkRoot;
    ANDROID_SDK_ROOT = sdkRoot;
    ANDROID_NDK_ROOT = ndkRoot;
    NDK_HOME = ndkRoot;
  };

  cargoVendorDir = rust.craneLib.vendorCargoDeps {inherit src;};

  cargoOffline = ''
    export CARGO_HOME=$TMPDIR/cargo
    mkdir -p "$CARGO_HOME"
    # -L DEREFERENCES. crane's vendor directory is nothing but symlinks into the
    # store — one per crate — so a plain `cp -r` copies the links and every
    # target is still read-only. The failure is then identical to having not
    # copied at all, except the path in the error looks writable, which is a
    # genuinely misleading place to end up.
    cp -rL ${cargoVendorDir} "$TMPDIR/vendor"
    chmod -R u+w "$TMPDIR/vendor"
    sed "s|${cargoVendorDir}|$TMPDIR/vendor|g" \
      ${cargoVendorDir}/config.toml > "$CARGO_HOME/config.toml"
  '';

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

      outputHashMode = "recursive";
      outputHashAlgo = "sha256";
      outputHash = lib.fakeHash;
    }
    // commonEnv
  );
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
        platforms = ["x86_64-linux"];
      };
    }
    // commonEnv
  )
