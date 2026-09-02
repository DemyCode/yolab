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

    rust = import ./nix/rust.nix {inherit pkgs inputs;};

    treefmtEval = inputs.treefmt-nix.lib.evalModule pkgs (
      import ./nix/treefmt.nix {inherit (rust) rustToolchain;}
    );

    # The config.toml path is an argument so the CI stubs can be evaluated
    # without a node's real config.toml being touched.
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
      # Guarded so `nix flake check` works on a clone that has no config.toml.
      # Where it is absent you get "flake output does not provide attribute"
      # rather than a readFile error three modules deep.
      // lib.optionalAttrs (builtins.pathExists ./homelab/ignored/config.toml) {
        yolab = mkYolabSystem {
          configPath = ./homelab/ignored/config.toml;
          modules = baseModules;
        };
      };

    bootTest = import ./nix/tests/boot.nix {
      inherit pkgs inputs rust disko;
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
          yolabConfigPath = ./homelab/ignored/config.toml;
        };
      };
  in {
    nixosConfigurations = nixosSystems;

    # VM tests that actually boot machines. Kept out of `checks` on purpose: a
    # boot test needs a QEMU-capable runner (CI has one, the build sandbox does
    # not) and has not yet been verified to pass, so it must not be part of
    # `nix flake check`. Run it explicitly:
    #   nix build .#nixosTests.boot-test
    nixosTests = {
      boot-test = bootTest;
    };

    # Guarded like `yolab`: these import shared.nix too. No CI stub variant,
    # because a Darwin toplevel cannot be built from x86_64-linux checks.
    darwinConfigurations = lib.optionalAttrs (builtins.pathExists ./homelab/ignored/config.toml) {
      "yolab-mac" = mkDarwinSystem "aarch64-darwin";
      "yolab-mac-x86" = mkDarwinSystem "x86_64-darwin";
    };

    # `coverage-*` filtered out: they are reports, not gates. See nix/checks.nix.
    checks.x86_64-linux = lib.filterAttrs (n: _: !lib.hasPrefix "coverage-" n) allChecks;

    formatter.x86_64-linux = treefmtEval.config.build.wrapper;

    packages.x86_64-linux = let
      builds = import ./homelab/builds.nix {inherit pkgs rust;};
      checks = self.checks.x86_64-linux;
    in {
      inherit (allChecks) coverage-local-api;
      inherit (allChecks) coverage-installer;

      # eslint is a package, not a check: 7 pre-existing findings. It belongs in
      # `checks` once those are fixed, as clippy now is.
      client-ui-lint = builds.clientUiLint;

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

        format = {
          type = "app";
          program = lib.getExe treefmtEval.config.build.wrapper;
          meta.description = "Format the whole tree";
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
  };
}
