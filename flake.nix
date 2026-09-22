{
  description = "Yolab";

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

    devour-flake = {
      url = "github:srid/devour-flake";
      flake = false;
    };

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

    machineConfig = "${inputs.yolab-machine}/config.toml";
    machineHardware = "${inputs.yolab-machine}/hardware-configuration.nix";
    isMachine = builtins.pathExists machineConfig;

    yolabSpecialArgs = configPath: {
      inherit rust;
      yolabConfigPath = configPath;
      localApiEnv = rust.crates.local-api.package;
      yolabRev = self.rev or self.dirtyRev or "";
      yolabLastModified = self.lastModified or null;
    };

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

    nixosTests = {
      boot-test = bootTest;
      two-node-test = twoNodeTest;
      disk-loss-test = diskLossTest;
    };

    darwinConfigurations = lib.optionalAttrs isMachine {
      "yolab-mac" = mkDarwinSystem "aarch64-darwin";
      "yolab-mac-x86" = mkDarwinSystem "x86_64-darwin";
    };

    checks.x86_64-linux =
      lib.filterAttrs (n: _: !lib.hasPrefix "coverage-" n) allChecks
      // self.nixosTests;

    formatter.x86_64-linux = treefmtEval.config.build.wrapper;

    packages.x86_64-linux = let
      builds = import ./homelab/builds.nix {inherit pkgs rust;};
      checks = self.checks.x86_64-linux;
    in {
      devour = pkgs.callPackage inputs.devour-flake {};

      inherit (allChecks) coverage-local-api;
      inherit (allChecks) coverage-installer;

      client-ui-lint = builds.clientUiLint;

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

      desktop-client = rust.crates.desktop-client.package;

      android-apk = import ./nix/android.nix {inherit pkgs rust;};

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

        desktop-client = {
          type = "app";
          program = "${self.packages.x86_64-linux.desktop-client}/bin/yolab-desktop";
          meta.description = "Open the YoLab desktop window";
        };
      }
      // lib.mapAttrs (name: drv: {
        type = "app";
        program = toString (pkgs.writeShellScript "check" "echo ${drv}");
        meta.description = "Build the ${name} check and print its store path";
      })
      self.checks.x86_64-linux;

    devShells.x86_64-linux.default = pkgs.mkShell {
      packages =
        (with pkgs; [
          statix
          deadnix
          shellcheck
          hadolint
          kubernetes-helm
          pkg-config
          openssl
          uv
          nodejs
          pre-commit
          busybox
          jq
          (python3.withPackages (ps: [ps.pyyaml]))
        ])
        ++ [rust.rustToolchain]
        ++ builtins.attrValues treefmtEval.config.build.programs
        ++ [treefmtEval.config.build.wrapper];

      shellHook = ''
        echo "yolab devshell — 'nix run .#ci' runs every check exactly as CI does"
      '';
    };

    devShells.x86_64-linux.desktop = pkgs.mkShell {
      packages =
        (with pkgs; [
          pkg-config
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
