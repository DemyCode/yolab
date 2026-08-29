{
  description = "Yolab";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # Ceph only. The pinned nixpkgs above carries ceph 20.2.2, whose
    # python-common fails its own pytest ("No package metadata was found for
    # ceph-common") — Hydra fails it too, so it is not in the binary cache and
    # every node would compile Ceph from source. 20.2.3 in nixpkgs-unstable is
    # fixed and cached. Scoped to a single package via an overlay rather than
    # bumping the main pin, so the blast radius is Ceph and nothing else.
    nixpkgs-ceph.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
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

    # The Rust toolchain and both crates, defined once and shared by the
    # checks, the packages, the devshell, the formatter and the ISO. Four
    # copies of this expression used to exist and one had already drifted —
    # see the header of rust.nix.
    rust = import ./rust.nix {inherit pkgs inputs;};

    # One formatter for the whole tree, and the check that gates it, both built
    # from treefmt.nix. Same wrapper, same config file, same pinned binaries,
    # so `nix fmt` and `checks.formatting` cannot disagree about what
    # formatted means. See the header of treefmt.nix.
    treefmtEval = inputs.treefmt-nix.lib.evalModule pkgs (
      import ./treefmt.nix {inherit (rust) rustToolchain;}
    );

    # A machine config. The config.toml path is an argument rather than a
    # path hardcoded in each module, which is what lets the CI stubs below be
    # evaluated without a node's real config.toml being touched. See
    # homelab/shared.nix.
    mkYolabSystem = {
      configPath,
      modules,
    }:
      nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        inherit modules;
        specialArgs = {
          inherit inputs rust;
          yolabConfigPath = configPath;
        };
      };

    baseModules = [
      disko.nixosModules.disko
      ./homelab/nixos/configuration.nix
      ./homelab/nixos/disk-config.nix
    ];

    # Every machine config that can be evaluated anywhere, plus `yolab`
    # itself when this checkout has a real config.toml. Defined here rather
    # than inline under `nixosConfigurations` so checks.nix can build their
    # toplevels without reaching back through `self`.
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
      # `yolab` is the real machine, and it only exists where its config.toml
      # does. Guarding it is what makes `nix flake check` work on a fresh
      # clone: without this the evaluation of every output fails on a missing
      # file. On a node the file is present and `#yolab` resolves as before;
      # where it is absent you get "flake output does not provide attribute"
      # instead of a readFile error from three modules deep.
      // lib.optionalAttrs (builtins.pathExists ./homelab/ignored/config.toml) {
        yolab = mkYolabSystem {
          configPath = ./homelab/ignored/config.toml;
          modules = baseModules;
        };
      };

    # Everything checks.nix defines, including the `coverage-*` reports that are
    # filtered back out of `checks` below.
    allChecks = import ./checks.nix {
      inherit
        pkgs
        inputs
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
          yolabConfigPath = ./homelab/ignored/config.toml;
        };
      };
  in {
    nixosConfigurations = nixosSystems;

    # Guarded the same way `yolab` is, and for the same reason: these import
    # shared.nix too, so without it a checkout with no config.toml fails to
    # evaluate them. There is no CI stub variant because a Darwin toplevel
    # cannot be built from the x86_64-linux checks.
    darwinConfigurations = lib.optionalAttrs (builtins.pathExists ./homelab/ignored/config.toml) {
      "yolab-mac" = mkDarwinSystem "aarch64-darwin";
      "yolab-mac-x86" = mkDarwinSystem "x86_64-darwin";
    };

    # Every check lives in checks.nix as a derivation, so the same command runs
    # them on a laptop and on a runner. `nix flake check` builds all of them.
    #
    # The `coverage-*` entries are filtered out: they are reports, not checks.
    # Leaving them in would make `nix flake check` build a full instrumented
    # rebuild of every crate, and would turn a coverage percentage into a thing
    # CI can fail on — which pushes people to write tests that move the number
    # instead of tests that catch bugs. They are exposed under packages instead.
    checks.x86_64-linux = lib.filterAttrs (n: _: !lib.hasPrefix "coverage-" n) allChecks;

    # `nix fmt`. The wrapper is the same derivation `checks.formatting` runs,
    # so formatting the tree and gating on it can never be two opinions.
    formatter.x86_64-linux = treefmtEval.config.build.wrapper;

    packages.x86_64-linux = let
      builds = import ./homelab/builds.nix {inherit pkgs inputs rust;};
      checks = self.checks.x86_64-linux;
    in {
      inherit (allChecks) coverage-local-api;
      inherit (allChecks) coverage-installer;

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

      # `nix run .#ci` — the whole suite. Every check is a build input of this
      # script, so nix has already built (i.e. run) all of them before the first
      # line executes: a failing check fails `nix run` itself, with that check's
      # own log. What this prints is therefore a summary of what passed, not a
      # test runner. `nix flake check` is equivalent and is what CI calls.
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
        };

        # Literally `nix fmt`: the same wrapper, reached by the verb people
        # expect when every other entry point in this repo is a `nix run`.
        format = {
          type = "app";
          program = lib.getExe treefmtEval.config.build.wrapper;
        };
      }
      // lib.mapAttrs (_: drv: {
        type = "app";
        program = toString (pkgs.writeShellScript "check" "echo ${drv}");
      })
      self.checks.x86_64-linux;

    # The same packages the checks and the formatter use, so a tool run by
    # hand here behaves exactly as it does inside a derivation. The toolchain
    # comes from rust.nix and the formatters come from treefmtEval, rather
    # than being a second list that has to be kept in step with them.
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
          # Rust (version from rust-toolchain.toml, via rust.nix)
          pkg-config
          openssl
          uv
          # Node.js
          nodejs
          # Runner
          pre-commit
          # Everything the checks need, so each can also be run by hand while
          # iterating: `cargo test`, `sh apps/wg-register/setup_test.sh`,
          # `python3 apps/catalog/check_charts.py`. `nix run .#ci` runs the lot
          # the way CI does.
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
  };
}
