{
  pkgs,
  inputs,
}: let
  rustToolchain = (pkgs.extend inputs.rust-overlay.overlays.default)
    .rust-bin.fromRustupToolchainFile ../homelab/local-api/rust-toolchain.toml;

  craneLib = (inputs.crane.mkLib pkgs).overrideToolchain rustToolchain;

  mkCrate = {
    pname,
    path,
    src ? null,
    nativeBuildInputs ? [],
    buildInputs ? [],
    extraArgs ? {},
    testsArgs ? {},
  }: let
    args =
      {
        inherit pname buildInputs;
        nativeBuildInputs = nativeBuildInputs ++ [pkgs.cacert];
        version = "0.1.0";
        src =
          if src != null
          then src
          else craneLib.cleanCargoSource (craneLib.path path);
        strictDeps = true;
        RUSTFLAGS = "-D warnings";
        SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
      }
      // extraArgs;

    cargoArtifacts = craneLib.buildDepsOnly args;

    covArgs = args // {pname = "${pname}-coverage";};
  in {
    inherit args cargoArtifacts;

    package = craneLib.buildPackage (args // {inherit cargoArtifacts;});
    tests = craneLib.cargoTest (args // {inherit cargoArtifacts;} // testsArgs);

    clippy = craneLib.cargoClippy (args
      // {
        inherit cargoArtifacts;
        pname = "${pname}-clippy";
        cargoClippyExtraArgs = "--all-targets -- -D warnings";
      });

    coverage = craneLib.cargoLlvmCov (covArgs
      // {
        cargoArtifacts = craneLib.buildDepsOnly covArgs;
        cargoLlvmCovCommand = "test";
        cargoLlvmCovExtraArgs = "--html --output-dir $out";
        postInstall = ''
          echo "── coverage summary: ${pname} ──"
          cargo llvm-cov report --release --summary-only \
            | tee "$out/coverage-summary.txt"
        '';
      });
  };
in {
  inherit rustToolchain craneLib;

  crates = {
    local-api = mkCrate {
      pname = "local-api";
      path = ../homelab/local-api;
      nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.bintools pkgs.gitMinimal];
      buildInputs = [pkgs.openssl];
      # The kube/wiremock tests bind loopback and reach it; a sandbox has no
      # network, so run this one test derivation with the host network.
      extraArgs.doCheck = false;
      testsArgs.__noChroot = true;
    };

    installer = mkCrate {
      pname = "yolab-installer";
      path = ../installer/nixos/backend-rs;
      nativeBuildInputs = [pkgs.pkg-config];
    };

    desktop-client = mkCrate {
      pname = "yolab-desktop";
      path = ../shells/desktop;

      src = pkgs.lib.cleanSourceWith {
        name = "yolab-desktop-source";
        src = ../shells/desktop;
        filter = path: type: !(type == "directory" && baseNameOf path == "target");
      };

      nativeBuildInputs = [pkgs.pkg-config pkgs.wrapGAppsHook3];

      buildInputs = with pkgs; [
        webkitgtk_4_1
        gtk3
        libsoup_3
        glib
        cairo
        pango
        gdk-pixbuf
        atk
        librsvg
        dbus
        openssl
        glib-networking
      ];

      extraArgs = {
        preFixup = ''
          gappsWrapperArgs+=(
            --set WEBKIT_DISABLE_DMABUF_RENDERER 1
            --prefix GIO_EXTRA_MODULES : "${pkgs.glib-networking}/lib/gio/modules"
          )
        '';
      };
    };
  };
}
