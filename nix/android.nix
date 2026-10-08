{
  pkgs,
  rust,
  signingKey ? null,
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
  buildTools = "${sdkRoot}/build-tools/36.0.0";

  keystore = "${signingKey}/yolab.jks";
  passwordFile = "${signingKey}/password";
  hasKey =
    signingKey
    != null
    && builtins.pathExists keystore
    && builtins.pathExists passwordFile;

  chooseKey =
    if hasKey
    then ''
      ks=${keystore}
      ks_pass=file:${passwordFile}
    ''
    else ''
      echo "no key in the yolab-android-key input: signing with a throwaway key, so this APK cannot update one signed with yours" >&2
      ks=$TMPDIR/throwaway.jks
      keytool -genkeypair -keystore "$ks" -alias yolab -keyalg RSA -keysize 2048 \
        -validity 10000 -dname "CN=YoLab throwaway" -storepass throwaway -keypass throwaway
      ks_pass=pass:throwaway
    '';

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

  aapt2Override = ''
    printf 'android.aapt2FromMavenOverride=%s\n' "${buildTools}/aapt2" \
      >> "$GRADLE_USER_HOME/gradle.properties"
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
        export GRADLE_USER_HOME=$TMPDIR/gradle
        ${cargoOffline}
        mkdir -p "$GRADLE_USER_HOME"
        ${aapt2Override}

        cargo tauri android init --ci
        ${gradlewShim}

        cargo tauri android build --apk || true
        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        mkdir -p $out

        cp -r "$GRADLE_USER_HOME"/caches/modules-2 $out/ 2>/dev/null || true

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
      outputHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=";
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
        export GRADLE_USER_HOME=$TMPDIR/gradle
        ${cargoOffline}
        mkdir -p "$GRADLE_USER_HOME/caches"
        cp -r ${gradleDeps}/modules-2 "$GRADLE_USER_HOME/caches/"
        chmod -R u+w "$GRADLE_USER_HOME"
        ${aapt2Override}

        cargo tauri android init --ci
        ${gradlewShim}
        cargo tauri android build --apk

        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        mkdir -p $out
        unsigned=$(find gen/android -name '*release*.apk' -print -quit)
        if [ -z "$unsigned" ]; then
          echo "no release APK was produced — the Gradle assemble step did not run" >&2
          exit 1
        fi
        ${chooseKey}
        ${buildTools}/zipalign -f -p 4 "$unsigned" "$TMPDIR/aligned.apk"
        ${buildTools}/apksigner sign --ks "$ks" --ks-key-alias yolab --ks-pass "$ks_pass" \
          --out $out/yolab.apk "$TMPDIR/aligned.apk"
        ${buildTools}/apksigner verify $out/yolab.apk
        runHook postInstall
      '';

      passthru = {inherit buildTools;};

      meta = {
        description = "YoLab as a signed Android APK (key from the yolab-android-key input)";
        platforms = ["x86_64-linux"];
      };
    }
    // commonEnv
  )
