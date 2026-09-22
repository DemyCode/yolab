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
    crane.url = "github:ipetkov/crane";
    rust-overlay.url = "github:oxalica/rust-overlay";
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    treefmt-nix.url = "github:numtide/treefmt-nix";
    treefmt-nix.inputs.nixpkgs.follows = "nixpkgs";

    yolab-machine = {
      url = "path:./homelab/machine";
      flake = false;
    };
  };

  outputs = {
    self,
    nixpkgs,
    disko,
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
  in {
    nixosConfigurations =
      {yolab-installer = nixosSystems.yolab-installer;}
      // lib.optionalAttrs isMachine {yolab = nixosSystems.yolab;};

    checks.x86_64-linux =
      allChecks
      // {
        boot-test = bootTest;
        two-node-test = twoNodeTest;
        disk-loss-test = diskLossTest;
      };

    formatter.x86_64-linux = treefmtEval.config.build.wrapper;

    packages.x86_64-linux = {
      desktop-client = rust.crates.desktop-client.package;
      android-apk = import ./nix/android.nix {inherit pkgs rust;};
    };

    apps.x86_64-linux = {
      desktop-client = {
        type = "app";
        program = "${self.packages.x86_64-linux.desktop-client}/bin/yolab-desktop";
        meta.description = "Open the YoLab desktop window";
      };
    };

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
        echo "yolab devshell"
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
