{
  pkgs,
  inputs,
}: let
  untested = import ./untested.nix;

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
    testInputs ? [],
    testEnv ? {},
    untested ? {},
  }: let
    inherit (pkgs) lib;
    untestedFiles = builtins.attrNames untested;
    ignoreUntested =
      lib.optionalString (untestedFiles != [])
      "--ignore-filename-regex ${lib.escapeShellArg (lib.concatMapStringsSep "|" (f: "/${lib.escapeRegex f}$") untestedFiles)}";
    excludeUntested = lib.concatMapStringsSep " " (f: "--exclude ${lib.escapeShellArg f}") untestedFiles;

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

    withTestTools = a: a // testEnv // {nativeBuildInputs = a.nativeBuildInputs ++ testInputs;};
    covArgs = withTestTools (args // {pname = "${pname}-coverage";});
    strict = args // {CARGO_BUILD_WARNINGS = "deny";};
  in {
    inherit args cargoArtifacts untested;

    package = craneLib.buildPackage (strict
      // {
        inherit cargoArtifacts;
        doCheck = false;
      });
    tests = craneLib.cargoTest (withTestTools strict // {inherit cargoArtifacts;});

    clippy = craneLib.cargoClippy (strict
      // {
        inherit cargoArtifacts;
        pname = "${pname}-clippy";
        cargoClippyExtraArgs = "--all-targets -- -D warnings";
      });

    coverage = craneLib.mkCargoDerivation (covArgs
      // {
        cargoArtifacts = craneLib.buildDepsOnly covArgs;
        nativeBuildInputs = covArgs.nativeBuildInputs ++ [pkgs.cargo-llvm-cov pkgs.jq];
        doInstallCargoArtifacts = false;
        buildPhaseCargoCommand = ''
          cargo llvm-cov --release --no-report
          YOLAB_TEST_LOGS=off cargo llvm-cov --release --no-report
          cargo llvm-cov report --release --html --output-dir "$out" ${ignoreUntested}
          cargo llvm-cov report --release --json --output-path regions.json ${ignoreUntested}
          echo "Uncovered regions (line:column):"
          jq -r '.data[0].files[]
            | { f: .filename, r: [ .segments[] | select(.[2] == 0 and .[3] and .[4] and (.[5] | not)) | "\(.[0]):\(.[1])" ] }
            | select(.r | length > 0)
            | "\(.f): \(.r | join(" "))"' regions.json
          echo "Uncovered regions in the best-covered instantiation of each function (line:column):"
          jq -r '.data[0].functions
            | group_by([.filenames[0], .regions[0][0], .regions[0][1]])
            | map(max_by([.regions[] | select(.[4] > 0)] | length))
            | map({ f: .filenames[0], r: [ .regions[] | select(.[4] == 0 and .[5] == 0 and .[7] == 0) | "\(.[0]):\(.[1])" ] })
            | map(select(.r | length > 0))
            | .[] | "\(.f): \(.r | join(" "))"' regions.json
          cargo llvm-cov report --release --summary-only --show-missing-lines ${ignoreUntested} \
            --fail-under-lines 100 --fail-under-functions 100 --fail-under-regions 100
        '';
        installPhaseCommand = "";
      });

    mutants = craneLib.mkCargoDerivation (withTestTools args
      // {
        pname = "${pname}-mutants";
        CARGO_BUILD_INCREMENTAL = "true";
        CARGO_PROFILE_DEV_DEBUG = "0";
        inherit cargoArtifacts;
        nativeBuildInputs = args.nativeBuildInputs ++ testInputs ++ [pkgs.cargo-mutants];
        doInstallCargoArtifacts = false;
        buildPhaseCargoCommand = ''
          mkdir -p "$out"
          cargo mutants --jobs 4 --copy-target=true --output "$out" ${excludeUntested}
        '';
        installPhaseCommand = "";
      });

    untestedMissing = lib.filter (f: !builtins.pathExists (path + "/${f}")) untestedFiles;
  };
in {
  inherit rustToolchain craneLib;

  crates = {
    local-api = mkCrate {
      pname = "local-api";
      path = ../homelab/local-api;
      src = pkgs.lib.cleanSourceWith {
        src = craneLib.path ../homelab/local-api;
        filter = path: type:
          craneLib.filterCargoSources path type || pkgs.lib.hasSuffix ".sh" path;
      };
      nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.bintools pkgs.gitMinimal];
      buildInputs = [pkgs.openssl];
      testInputs = [pkgs.kubernetes-helm];
      testEnv.HELM_CACHE_HOME = "/build/.helm/cache";
      testEnv.HELM_CONFIG_HOME = "/build/.helm/config";
      testEnv.HELM_DATA_HOME = "/build/.helm/data";
      testEnv.YOLAB_TEST_CATALOG = pkgs.linkFarm "yolab-test-catalog" [
        {
          name = "yolab-common";
          path = ../apps/catalog/yolab-common;
        }
        {
          name = "pairdrop";
          path = ../apps/catalog/pairdrop;
        }
      ];
      untested = untested.local-api;
    };

    installer = mkCrate {
      pname = "yolab-installer";
      path = ../installer/nixos/backend-rs;
      nativeBuildInputs = [pkgs.pkg-config];
      untested = untested.installer;
    };

    desktop-client = mkCrate {
      pname = "yolab-desktop";
      path = ../shells/desktop;
      untested = untested.desktop-client;

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
