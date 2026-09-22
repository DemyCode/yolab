{
  description = "Yolab";

  # `extra-` prefix, not `substituters`/`trusted-public-keys`: those APPEND to
  # whatever the building machine already trusts (cache.nixos.org included) —
  # the bare names would REPLACE the whole list, silently dropping every
  # ordinary nixpkgs package back to building from source.
  #
  # Only takes effect with `accept-flake-config = true` on the building side
  # (Nix asks interactively otherwise, and CI has nothing to answer that
  # prompt with) — see .github/workflows/push.yml's Install Nix step, and
  # homelab/nixos/common.nix for real deployed nodes, which set
  # nix.settings.substituters directly instead since that config is trusted
  # by construction rather than needing this opt-in.
  #
  # The cache itself is public: pulling from it needs no token, only pushing
  # does (see push.yml's "Push to the Nix cache" step) — so nothing sensitive
  # lives here.
  nixConfig = {
    extra-substituters = ["https://cache.yolab.io/yolab"];
    extra-trusted-public-keys = ["yolab:3CIkfuGsBgTSWSAZJ2FCbVXjLG1RwNJvvGS1MAtQCmQ="];
  };

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";
    nixos-wsl.url = "github:nix-community/NixOS-WSL";
    nixos-wsl.inputs.nixpkgs.follows = "nixpkgs";
    nix-darwin.url = "github:LnL7/nix-darwin";
    nix-darwin.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";

    # Builds EVERY flake output — packages, apps, checks (VM tests included),
    # devShells, nixosConfigurations — in one `nix build`, so CI no longer names
    # what to build: it builds whatever the flake exposes. Non-flake input
    # because it is consumed as a package (`pkgs.callPackage`), not as a flake.
    #
    # See `packages.<system>.devour` below and the `ci` job in push.yml. The
    # point is that adding a package, check or devShell needs no CI change.
    devour-flake = {
      url = "github:srid/devour-flake";
      flake = false;
    };

    # THIS MACHINE's own files: config.toml (secrets, tunnel keys, tokens) and
    # hardware-configuration.nix. They are not in the repo, so a machine builds
    # from a flake URL and keeps these as the only local files — the node no
    # longer holds a checkout of the repo at all. The channel in local-api names
    # the URL and ref; see homelab/local-api/src/config.rs.
    #
    # The default is an empty directory in the repo, which defines no machine: CI
    # and a fresh clone evaluate without any secrets. A machine points it at its
    # own directory on every build:
    #
    #   nixos-rebuild switch --flake github:DemyCode/yolab/main#yolab \
    #     --override-input yolab-machine path:/var/lib/yolab/machine \
    #     --no-write-lock-file
    #
    # --no-write-lock-file keeps the machine's files out of flake.lock.
    yolab-machine = {
      url = "path:./homelab/machine";
      flake = false;
    };
  };

  outputs = {
    self,
    nixpkgs,
    disko,
    nixos-wsl,
    nix-darwin,
    ...
  } @ inputs: let
    pkgs = nixpkgs.legacyPackages.x86_64-linux;
    inherit (nixpkgs) lib;

    rust = import ./nix/rust.nix {inherit pkgs inputs;};

    treefmtEval = inputs.treefmt-nix.lib.evalModule pkgs (
      import ./nix/treefmt.nix {inherit (rust) rustToolchain;}
    );

    # The machine's files, when `yolab-machine` points at a real machine (see the
    # input's comment). The in-repo default holds neither.
    machineConfig = "${inputs.yolab-machine}/config.toml";
    machineHardware = "${inputs.yolab-machine}/hardware-configuration.nix";
    isMachine = builtins.pathExists machineConfig;

    # EVERY MODULE ARGUMENT A YOLAB SYSTEM NEEDS, IN ONE PLACE.
    #
    # `nixosSystem` and the VM tests in nix/tests/ both build the same modules,
    # so both have to hand them the same arguments — and a missing one is not a
    # build failure anywhere near the test. It surfaces from inside nixpkgs'
    # module system as `attribute 'localApiEnv' missing`, naming nothing in the
    # file that forgot it, because nothing references these until a module deep
    # in common.nix builds a unit's ExecStart or an activation script from them.
    #
    # All three VM tests were failing on exactly that, twice over: boot.nix had
    # never passed `localApiEnv`, and none of the three passed `yolabRev` or
    # `yolabLastModified`. Every one of them died during evaluation, so not one
    # had ever run. A test nobody can run is worse than no test — it reads like
    # coverage.
    #
    # So the tests take this function rather than copying its body. Adding an
    # argument here reaches them in the same commit.
    yolabSpecialArgs = configPath: {
      inherit rust;
      yolabConfigPath = configPath;
      localApiEnv = rust.crates.local-api.package;
      # The revision this system is built from, for the version shown on the
      # System page. A node builds from a flake URL and keeps no git tree, so
      # the flake itself is the only place the revision can come from.
      yolabRev = self.rev or self.dirtyRev or "";
      yolabLastModified = self.lastModified or null;
    };

    # The config.toml path is an argument so the CI stubs can be evaluated
    # without a node's real config.toml being touched.
    mkYolabSystem = {
      configPath,
      modules,
    }:
      nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        inherit modules;
        specialArgs = yolabSpecialArgs configPath;
      };

    baseModules = [
      disko.nixosModules.disko
      ./homelab/nixos/configuration.nix
      ./homelab/nixos/disk-config.nix
    ];

    # Bound here rather than inline under `nixosConfigurations` so nix/checks.nix
    # can build their toplevels without reaching back through `self`.
    nixosSystems =
      {
        yolab-ci = mkYolabSystem {
          configPath = ./homelab/ci-config.toml;
          modules = baseModules;
        };
        yolab-ci-join = mkYolabSystem {
          configPath = ./homelab/ci-join-config.toml;
          modules = baseModules;
        };
        yolab-wsl = mkYolabSystem {
          configPath = ./homelab/ci-config.toml;
          modules = [
            nixos-wsl.nixosModules.default
            ./homelab/nixos/wsl.nix
          ];
        };
        yolab-installer = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
            ./installer/nixos/iso-config.nix
          ];
          specialArgs = {inherit inputs rust;};
        };
      }
      # Only when `yolab-machine` points at a real machine. Without the override
      # you get "flake output does not provide attribute 'yolab'" rather than a
      # readFile error three modules deep — and CI, which never overrides it, can
      # run `nix flake check` without any machine's secrets.
      // lib.optionalAttrs isMachine {
        yolab = mkYolabSystem {
          configPath = machineConfig;
          modules =
            baseModules
            ++ lib.optional (builtins.pathExists machineHardware) machineHardware;
        };
      };

    bootTest = import ./nix/tests/boot.nix {
      inherit
        pkgs
        inputs
        disko
        yolabSpecialArgs
        ;
    };

    twoNodeTest = import ./nix/tests/two-node.nix {
      inherit
        pkgs
        inputs
        disko
        yolabSpecialArgs
        ;
    };

    diskLossTest = import ./nix/tests/disk-loss.nix {
      inherit
        pkgs
        inputs
        disko
        yolabSpecialArgs
        ;
    };

    allChecks = import ./nix/checks.nix {
      inherit
        pkgs
        treefmtEval
        rust
        nixosSystems
        ;
    };

    mkDarwinSystem = system:
      nix-darwin.lib.darwinSystem {
        inherit system;
        modules = [./homelab/darwin/configuration.nix];
        specialArgs = {
          inherit inputs rust;
          yolabConfigPath = machineConfig;
        };
      };
  in {
    nixosConfigurations = nixosSystems;

    # VM tests that actually boot machines.
    #
    # KEPT OUT OF `checks` FOR TIME, NOT FOR CAPABILITY. This comment used to
    # say the build sandbox has no /dev/kvm, and that is simply false: a NixOS
    # VM test derivation carries `requiredSystemFeatures = [ "kvm" ]`, and any
    # builder advertising the `kvm` system feature hands it the device inside
    # the sandbox. Verified on 2026-09-22 by watching boot-test boot a machine
    # under `nix build` with `sandbox = true`.
    #
    # The real reason is wall clock. `nix flake check` builds every one of the
    # checks with no way to select a subset, and folding three VM boots into it
    # would turn the one command everybody runs before pushing into a
    # half-hour one. So they stay their own output, and `nix run .#test -- boot`
    # is how you ask for one by name.
    #
    # The cost of that choice is the `warning: unknown flake output 'nixosTests'`
    # every `nix flake check` prints — nix knows nothing about this attribute, so
    # it neither builds nor type-checks it. CI does instead: the `vm` job in
    # .github/workflows/push.yml builds its matrix from `builtins.attrNames` of
    # THIS attribute set, one runner each, so adding a test here is all it takes
    # to get it run.
    #
    # Locally:
    #   nix run .#test -- boot-test
    #   nix build .#nixosTests.two-node-test
    #
    # two-node-test is the one to run before shipping anything that touches
    # Ceph, k3s ordering or the image store: two machines is the topology most
    # installs actually have, and it is the topology where a node taking itself
    # offline to do maintenance costs the whole cluster its etcd quorum. Every
    # storage bug this project has had was found on a live two-node cluster
    # rather than here, which is the wrong order.
    nixosTests = {
      boot-test = bootTest;
      two-node-test = twoNodeTest;
      # Unplugs a disk from a one-copy cluster and FORCE HEALs it.
      disk-loss-test = diskLossTest;
    };

    # Guarded like `yolab`: these import shared.nix too. No CI stub variant,
    # because a Darwin toplevel cannot be built from x86_64-linux checks.
    darwinConfigurations = lib.optionalAttrs isMachine {
      "yolab-mac" = mkDarwinSystem "aarch64-darwin";
      "yolab-mac-x86" = mkDarwinSystem "x86_64-darwin";
    };

    # EVERYTHING, INCLUDING THE MACHINES.
    #
    # `nix flake check` takes no filter — it is every derivation in here or
    # none — so what goes in this attribute decides what "the flake is correct"
    # is allowed to mean. The VM tests used to sit outside it, which made a green
    # `nix flake check` a statement about 28 static checks and nothing about
    # whether a machine boots. That is the weaker claim, and it is not the one
    # worth making.
    #
    # They run in the build sandbox perfectly well: a NixOS VM test carries
    # `requiredSystemFeatures = [ "kvm" ]` and any builder advertising the `kvm`
    # system feature hands it /dev/kvm. What it costs is wall clock — this
    # command is now tens of minutes, not one — and it will FAIL on a machine
    # with no KVM rather than skip. Both are the point: a check that quietly
    # does not run is the thing this repo keeps getting caught by.
    #
    # `nix run .#test -- <filter>` is the fast selective path for day-to-day
    # work; this is the one that has to be green before shipping.
    #
    # `coverage-*` stays out: it is a report, not a gate. See nix/checks.nix.
    checks.x86_64-linux =
      lib.filterAttrs (n: _: !lib.hasPrefix "coverage-" n) allChecks
      // self.nixosTests;

    formatter.x86_64-linux = treefmtEval.config.build.wrapper;

    packages.x86_64-linux = let
      builds = import ./homelab/builds.nix {inherit pkgs rust;};
      checks = self.checks.x86_64-linux;
    in {
      # `nix run .#devour -- .` builds every output of THIS flake (or of any
      # flake URL passed instead) in one `nix build`, then prints a JSON of the
      # resulting store paths. It is the whole of CI's build step: the workflow
      # runs this and pushes the closure, so a new package, check or devShell
      # reaches the cache without anyone editing the workflow.
      devour = pkgs.callPackage inputs.devour-flake {};

      inherit (allChecks) coverage-local-api;
      inherit (allChecks) coverage-installer;

      # eslint is a package, not a check: 7 pre-existing findings. It belongs in
      # `checks` once those are fixed, as clippy now is.
      client-ui-lint = builds.clientUiLint;

      # `nix run .#test` — every check, or the ones whose name matches.
      #
      # `nix flake check` builds every check and has no filter of any kind: it
      # is all 28 or nothing, and it does NOT touch the three VM tests, which
      # live under `nixosTests` because they need /dev/kvm. So a green
      # `nix flake check` is not the same as "everything is tested", and there
      # is no flag that makes it so.
      #
      #   nix run .#test              # every check (not the VM tests)
      #   nix run .#test -- rust      # local-api-tests, clippy-*, ...
      #   nix run .#test -- ceph      # ceph-survives-a-rebuild
      #   nix run .#test -- boot      # boot-test, a real VM (needs KVM)
      #   nix run .#test -- --list    # what there is to match against
      #
      # Matching is a plain substring over both sets, so one name is as easy to
      # reach as a family of them, and everything selected is built in ONE
      # `nix build` — nix then realises shared dependencies once instead of per
      # check, which is the whole reason CI buckets exist.
      test = pkgs.writeShellApplication {
        name = "yolab-test";
        runtimeInputs = [pkgs.nix];
        text = ''
          # `checks` contains the VM tests too now (see checks.x86_64-linux), so
          # the static ones are the difference. Without subtracting, --list
          # would show each VM test twice and read like there are six.
          checks="${lib.concatStringsSep " " (lib.subtractLists (builtins.attrNames self.nixosTests) (builtins.attrNames self.checks.x86_64-linux))}"
          vms="${lib.concatStringsSep " " (builtins.attrNames self.nixosTests)}"
          filter="''${1-}"

          if [ "$filter" = "--list" ]; then
            echo "static checks:"
            for n in $checks; do echo "  $n"; done
            echo "VM tests (also in nix flake check; need /dev/kvm):"
            for n in $vms; do echo "  $n"; done
            exit 0
          fi

          targets=()
          for n in $checks; do
            case "$n" in *"$filter"*) targets+=(".#checks.x86_64-linux.$n") ;; esac
          done
          # Only when asked for by name. An unfiltered run must not quietly
          # start booting virtual machines.
          if [ -n "$filter" ]; then
            for n in $vms; do
              case "$n" in *"$filter"*) targets+=(".#nixosTests.$n") ;; esac
            done
          fi

          if [ ''${#targets[@]} -eq 0 ]; then
            echo "nothing matches '$filter' — try: nix run .#test -- --list" >&2
            exit 1
          fi

          echo "building ''${#targets[@]}:"
          printf '  %s\n' "''${targets[@]}"
          exec nix build --no-link --print-build-logs "''${targets[@]}"
        '';
      };

      # `nix run .#coverage` — build both HTML reports and say where they are.
      # Kept out of `ci` deliberately; see the note on checks.x86_64-linux.
      coverage = pkgs.writeShellApplication {
        name = "yolab-coverage";
        text = ''
          # cargo-llvm-cov writes its report tree under html/.
          echo "Browsable reports:"
          echo "  local-api  ${allChecks.coverage-local-api}/html/index.html"
          echo "  installer  ${allChecks.coverage-installer}/html/index.html"
          echo
          for r in ${allChecks.coverage-local-api} ${allChecks.coverage-installer}; do
            [ -f "$r/coverage-summary.txt" ] && cat "$r/coverage-summary.txt"
            echo
          done
        '';
      };

      iso = self.nixosConfigurations.yolab-installer.config.system.build.isoImage;
      homelab-ui = builds.clientUi;
      homelab-api = builds.localApiEnv;

      # `nix build .#desktop-client` produces the binary; `nix run` below opens
      # the window. It asks for the box address once and from then on opens
      # straight into it — see shells/desktop/README.md for why the UI is loaded
      # from the box rather than bundled here.
      desktop-client = rust.crates.desktop-client.package;

      # `nix build .#android-apk` -> ./result/*.apk, unsigned, for sideloading.
      #
      # NOT in `checks`, unlike desktop-client. It needs the Android SDK and NDK
      # — several gigabytes — and a Gradle dependency cache whose hash has to be
      # pinned by hand; putting that on every push would make CI slow and
      # brittle for an artifact that is cut on release, not on commit.
      android-apk = import ./nix/android.nix {inherit pkgs rust;};

      # Every check is a build input, so nix has already run them all before the
      # first line executes: this prints a summary, it is not a test runner.
      ci = pkgs.writeShellApplication {
        name = "yolab-ci";
        text = ''
          ${lib.concatMapStringsSep "\n" (name: ''
            echo "✓ ${name}  (${checks.${name}})"
          '') (builtins.attrNames checks)}
          echo "all ${toString (builtins.length (builtins.attrNames checks))} checks passed"
        '';
      };
    };

    # `nix run .#<check>` for a single one, and `nix run .` for everything.
    apps.x86_64-linux =
      {
        default = {
          type = "app";
          program = lib.getExe self.packages.x86_64-linux.ci;
          meta.description = "Run every check, exactly as CI does";
        };

        test = {
          type = "app";
          program = lib.getExe self.packages.x86_64-linux.test;
          meta.description = "Build the checks whose name matches (or all of them)";
        };

        format = {
          type = "app";
          program = lib.getExe treefmtEval.config.build.wrapper;
          meta.description = "Format the whole tree";
        };

        # Spelled out rather than `lib.getExe`, which requires meta.mainProgram
        # — crane does not set it, and getExe's failure is an eval error about a
        # missing attribute rather than anything about this app.
        desktop-client = {
          type = "app";
          program = "${self.packages.x86_64-linux.desktop-client}/bin/yolab-desktop";
          meta.description = "Open the YoLab desktop window";
        };
      }
      # meta.description silences a "lacks attribute meta" warning per app.
      // lib.mapAttrs (name: drv: {
        type = "app";
        program = toString (pkgs.writeShellScript "check" "echo ${drv}");
        meta.description = "Build the ${name} check and print its store path";
      })
      self.checks.x86_64-linux;

    # Toolchain from nix/rust.nix and formatters from treefmtEval, so a tool run
    # by hand behaves exactly as it does inside a derivation.
    devShells.x86_64-linux.default = pkgs.mkShell {
      packages =
        (with pkgs; [
          # Nix
          statix
          deadnix
          # Shell / Docker
          shellcheck
          hadolint
          # Apps are Helm charts — needed to lint/template them locally.
          kubernetes-helm
          # Rust (version from rust-toolchain.toml, via nix/rust.nix)
          pkg-config
          openssl
          uv
          # Node.js
          nodejs
          # Runner
          pre-commit
          # So each check can also be run by hand while iterating.
          busybox
          jq
          (python3.withPackages (ps: [ps.pyyaml]))
        ])
        ++ [rust.rustToolchain]
        # alejandra, rustfmt, prettier and shfmt at the exact versions
        # `nix fmt` uses, plus `treefmt` itself. The pre-commit alejandra hook
        # therefore runs the same binary the formatting check does.
        ++ builtins.attrValues treefmtEval.config.build.programs
        ++ [treefmtEval.config.build.wrapper];

      shellHook = ''
        echo "yolab devshell — 'nix run .#ci' runs every check exactly as CI does"
      '';
    };

    # The desktop shell (shells/desktop) needs a webview and its GTK stack, none
    # of which the default shell carries — and it is the whole toolchain cost of
    # that app: the Rust in it is a few hundred lines, while `cargo check` there
    # fails in the default shell on libdbus before it reaches a line of ours.
    #
    # Separate rather than merged into `default` so everyday work on local-api
    # and the charts does not pull webkitgtk and its closure.
    devShells.x86_64-linux.desktop = pkgs.mkShell {
      packages =
        (with pkgs; [
          pkg-config
          # Tauri v2 on Linux links against the 4.1 ABI specifically; 4.0 is
          # present in nixpkgs too and produces a confusing pkg-config miss.
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
        ])
        ++ [rust.rustToolchain];

      shellHook = ''
        echo "yolab desktop shell — cd shells/desktop && cargo check"
      '';
    };
  };
}
