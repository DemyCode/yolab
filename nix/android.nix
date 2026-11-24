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
      "37.0"
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

  cargoHome = ''
    export CARGO_HOME=$TMPDIR/cargo
    mkdir -p "$CARGO_HOME"
    sed "s|${cargoVendorDir}|$TMPDIR/vendor|g" \
      ${cargoVendorDir}/config.toml > "$CARGO_HOME/config.toml"
  '';

  cargoOffline = ''
    cp -rL ${cargoVendorDir} "$TMPDIR/vendor"
    chmod -R u+w "$TMPDIR/vendor"
    ${cargoHome}
  '';

  gradleSetup = ''
    export HOME=$TMPDIR
    export ANDROID_USER_HOME=$TMPDIR/.android
    export GRADLE_USER_HOME=$TMPDIR/gradle
    mkdir -p "$GRADLE_USER_HOME/caches"
    cp -r ${gradleDeps}/modules-2 "$GRADLE_USER_HOME/caches/"
    chmod -R u+w "$GRADLE_USER_HOME"
    ${aapt2Override}
  '';

  stubSrc = pkgs.runCommand "yolab-desktop-source" {} ''
    mkdir -p $out/src $out/ui $out/icons
    cp ${stubInputs}/Cargo.toml ${stubInputs}/Cargo.lock ${stubInputs}/build.rs ${stubInputs}/tauri.conf.json $out/
    cp ${stubInputs}/icons/icon.png $out/icons/
    echo '<!doctype html><title>YoLab</title>' > $out/ui/index.html
    cat > $out/src/lib.rs <<'EOF'
    #[cfg_attr(mobile, tauri::mobile_entry_point)]
    pub fn run() {
        tauri::Builder::default()
            .run(tauri::generate_context!())
            .expect("stub");
    }
    EOF
    echo 'fn main() { yolab_desktop_lib::run() }' > $out/src/main.rs
  '';

  stubInputs = pkgs.lib.fileset.toSource {
    root = ../shells/desktop;
    fileset = pkgs.lib.fileset.unions [
      ../shells/desktop/Cargo.toml
      ../shells/desktop/Cargo.lock
      ../shells/desktop/build.rs
      ../shells/desktop/tauri.conf.json
      ../shells/desktop/icons/icon.png
    ];
  };

  rustBuildCache = pkgs.stdenv.mkDerivation (
    {
      pname = "yolab-android-rust-build-cache";
      version = "0.1.0";
      src = stubSrc;
      inherit nativeBuildInputs;

      buildPhase = ''
        runHook preBuild
        ${cargoOffline}
        ${gradleSetup}
        cargo tauri android init --ci
        ${gradlewShim}
        cargo tauri android build --apk
        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        rm -rf gen/android/app/build gen/android/build gen/android/.gradle
        mkdir -p $out
        tar --zstd -cf $out/build.tar.zst -C "$TMPDIR" "$(basename "$PWD")" vendor
        runHook postInstall
      '';

      dontFixup = true;
    }
    // commonEnv
  );

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
    pkgs.gradle_9
    pkgs.pkg-config
    pkgs.nodejs
    pkgs.zstd
  ];

  gradleDeps = pkgs.stdenv.mkDerivation (
    {
      pname = "yolab-android-gradle-deps";
      version = "0.1.0";
      inherit src nativeBuildInputs;

      buildPhase = ''
        runHook preBuild
        export HOME=$TMPDIR
        export ANDROID_USER_HOME=$TMPDIR/.android
        export GRADLE_USER_HOME=$TMPDIR/gradle
        ${cargoOffline}
        mkdir -p "$GRADLE_USER_HOME"
        ${aapt2Override}

        cargo tauri android init --ci
        ${gradlewShim}

        cargo tauri android build --apk
        runHook postBuild
      '';

      installPhase = ''
        runHook preInstall
        mkdir -p $out

        cp -r "$GRADLE_USER_HOME"/caches/modules-2 $out/

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
      outputHash = "sha256-l30C1Yrpbg0VPh+rJ6fUh/DaHywy/4epeXO2exVargk=";
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
        here=$(basename "$PWD")
        cd "$TMPDIR"
        mv "$here" real-src
        tar --zstd -xf ${rustBuildCache}/build.tar.zst
        rm -rf "$here/src" "$here/ui"
        cp -r real-src/. "$here/"
        rm -rf real-src
        cd "$here"
        rm -rf target/release/.fingerprint/yolab-desktop-* target/*/release/.fingerprint/yolab-desktop-*
        find src ui build.rs -exec touch {} +
        ${cargoHome}
        ${gradleSetup}

        touch "$TMPDIR/before-icons"
        cargo tauri icon icons/icon.png
        new_icons=$(find gen/android -path '*/res/*' -name 'ic_launcher*.png' -newer "$TMPDIR/before-icons")
        if [ -z "$new_icons" ]; then
          echo "cargo tauri icon wrote no Android launcher icons under gen/android" >&2
          exit 1
        fi
        echo "$new_icons"
        bg_files=$(grep -rl 'name="ic_launcher_background"' gen/android --include='*.xml' || [ $? -eq 1 ])
        if [ -n "$bg_files" ]; then
          sed -i 's|\(name="ic_launcher_background"[^>]*>\)[^<]*|\1#2B56F5|' $bg_files
          grep -H 'name="ic_launcher_background"' $bg_files >&2
        else
          echo "no ic_launcher_background colour in gen/android: the launcher shows the full tile icon" >&2
        fi
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
