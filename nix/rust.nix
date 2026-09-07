# The Rust toolchain and both crates, defined once — the checks, the packages,
# the devshell, the formatter and the ISO all read from here, so none of them
# can end up on a different rustc than the others.
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
    # Overrides the default `cleanCargoSource`, which keeps only Rust and Cargo
    # files. That default is right for a crate whose source IS its .rs files and
    # wrong for one that compiles other things in — see `desktop-client`, whose
    # build script embeds an HTML page and an icon that cleanCargoSource would
    # silently drop, failing the build inside a proc macro with a missing-file
    # error a long way from the cause.
    src ? null,
    nativeBuildInputs ? [],
    buildInputs ? [],
    # Merged into every derivation this produces. For anything that is a
    # property of the crate itself rather than of one build of it — see
    # `desktop-client`, which needs a runtime environment variable set on the
    # wrapper regardless of whether it is being built, tested or linted.
    extraArgs ? {},
  }: let
    args =
      {
        inherit pname nativeBuildInputs buildInputs;
        version = "0.1.0";
        src =
          if src != null
          then src
          else craneLib.cleanCargoSource (craneLib.path path);
        strictDeps = true;
        # Registry crates get --cap-lints allow from cargo, so this only binds
        # our own code.
        RUSTFLAGS = "-D warnings";
      }
      // extraArgs;

    cargoArtifacts = craneLib.buildDepsOnly args;

    # Coverage needs its own dependency build: cargo-llvm-cov compiles with
    # instrumentation flags, so artifacts built without them either get
    # rebuilt or produce a report missing the dependencies' inlined code.
    covArgs = args // {pname = "${pname}-coverage";};
  in {
    inherit args cargoArtifacts;

    package = craneLib.buildPackage (args // {inherit cargoArtifacts;});
    tests = craneLib.cargoTest (args // {inherit cargoArtifacts;});

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
        # --summary-only cannot be combined with --html, so the report is built
        # here and the summary below reuses the same profile data.
        cargoLlvmCovExtraArgs = "--html --output-dir $out";
        # --release must match the test run, or `report` looks in the debug
        # target dir and silently finds nothing.
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
      # bintools for the profiler runtime coverage shells out to. gitMinimal
      # because routers/update.rs's remote-management tests run real git
      # against a throwaway repo rather than mocking the subprocess away —
      # without it, those tests fail only inside `cargo test`'s Nix sandbox,
      # where nothing outside declared inputs is on PATH.
      nativeBuildInputs = [pkgs.pkg-config pkgs.llvmPackages.bintools pkgs.gitMinimal];
      buildInputs = [pkgs.openssl];
    };

    # Only ever runs on a stranger's bare metal, so its tests are the one place
    # it is exercised at all.
    installer = mkCrate {
      pname = "yolab-installer";
      path = ../installer/nixos/backend-rs;
      nativeBuildInputs = [pkgs.pkg-config];
    };

    # The desktop window (shells/desktop). A webview pointed at the owner's box,
    # so it carries no UI of its own beyond the address prompt.
    desktop-client = mkCrate {
      pname = "yolab-desktop";
      path = ../shells/desktop;

      # tauri-build embeds index.html, tauri.conf.json and icons/icon.png INTO
      # the binary at compile time. cleanCargoSource keeps only Cargo and Rust
      # files, so with the default source this fails inside
      # `tauri::generate_context!` with "failed to open icon ... No such file",
      # which reads like a packaging bug rather than a filter dropping inputs.
      #
      # `target` is excluded because a local `cargo build` leaves gigabytes
      # there, and including it would both bust the derivation hash on every
      # local build and copy the lot into the store.
      src = pkgs.lib.cleanSourceWith {
        name = "yolab-desktop-source";
        src = ../shells/desktop;
        # Matched on the name rather than a path prefix: under a flake the src
        # is already a store path, so comparing against `toString ../…` depends
        # on two different spellings agreeing. This crate has no legitimate
        # directory called `target`, so the simpler rule is also the safer one.
        filter = path: type: !(type == "directory" && baseNameOf path == "target");
      };

      # wrapGAppsHook3 is what makes the built binary actually run: a GTK app
      # needs GIO modules, gdk-pixbuf loaders and GSettings schemas found
      # through environment variables, and without the wrapper it starts and
      # then fails to render anything.
      nativeBuildInputs = [pkgs.pkg-config pkgs.wrapGAppsHook3];

      # Tauri v2 links the 4.1 webkit ABI specifically. nixpkgs also ships
      # webkitgtk_4_0, and picking it produces a pkg-config miss whose message
      # names a package that is plainly installed.
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
        # THE THING THAT MAKES HTTPS WORK.
        #
        # WebKitGTK does not do TLS itself; it asks GIO, and GIO's TLS backend
        # is a loadable module that ships in glib-networking. Without it every
        # https:// request fails with "TLS support is not available" — so the
        # window opens perfectly and can never reach the box, which is the only
        # thing this app does.
        #
        # Being in buildInputs is not sufficient on its own; the module has to
        # be found at RUNTIME through GIO_EXTRA_MODULES, which is set on the
        # wrapper below.
        glib-networking
      ];

      extraArgs = {
        # WebKitGTK 2.42 renders through a DMABUF path that fails outright on a
        # good number of compositor and driver combinations. When it does, the
        # app builds, links, launches and then dies before drawing anything:
        #
        #   Gdk-Message: Error 71 (Protocol error) dispatching to Wayland display
        #
        # Observed on this very repo's first run. Turning the path off costs a
        # rendering optimisation nobody would notice in a settings window, and
        # several distributions ship exactly this as a default.
        #
        # Deliberately NOT GDK_BACKEND=x11, which also cures it: that forces the
        # whole app through XWayland and gives up native Wayland, fractional
        # scaling included, to fix a renderer bug.
        # GIO_EXTRA_MODULES is set explicitly rather than left to
        # wrapGAppsHook3's own detection. The hook infers module paths from a
        # derivation's inputs, and with crane's install phase that inference
        # does not reliably fire — the symptom being a window that opens and
        # then cannot make a single HTTPS request.
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
