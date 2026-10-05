{
  description = "Yolab";

  nixConfig = {
    extra-substituters = ["https://cache.yolab.io/yolab"];
    extra-trusted-public-keys = ["yolab:3CIkfuGsBgTSWSAZJ2FCbVXjLG1RwNJvvGS1MAtQCmQ="];
    sandbox = "relaxed";
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
    yolab-android-key = {
      url = "path:./homelab/android-key";
      flake = false;
    };
  };

  outputs = {
    self,
    nixpkgs,
    disko,
    ...
  } @ inputs: let
    systems = ["x86_64-linux" "aarch64-linux"];
    pkgsFor = system: nixpkgs.legacyPackages.${system};
    rustFor = lib.genAttrs systems (system:
      import ./nix/rust.nix {
        pkgs = pkgsFor system;
        inherit inputs;
      });

    pkgs = pkgsFor "x86_64-linux";
    inherit (nixpkgs) lib;

    rust = rustFor.x86_64-linux;

    machineConfig = "${inputs.yolab-machine}/config.toml";
    machineHardware = "${inputs.yolab-machine}/hardware-configuration.nix";
    machineFacter = "${inputs.yolab-machine}/facter.json";
    isMachine = builtins.pathExists machineConfig;
    hasFacter = builtins.pathExists machineFacter;

    machineSystem = import ./nix/machine-system.nix {
      config =
        if isMachine
        then builtins.fromTOML (builtins.readFile machineConfig)
        else {};
      facter =
        if hasFacter
        then lib.importJSON machineFacter
        else {};
      inherit systems;
    };

    specialArgsFor = system: configPath: {
      rust = rustFor.${system};
      yolabConfigPath = configPath;
      yolabFacterPath = null;
      localApiEnv = rustFor.${system}.crates.local-api.package;
      yolabRev = self.rev or self.dirtyRev or "";
      yolabLastModified = self.lastModified or null;
    };

    mkYolabSystem = {
      system ? "x86_64-linux",
      configPath,
      facterPath ? null,
      modules,
    }:
      nixpkgs.lib.nixosSystem {
        inherit system modules;
        specialArgs = specialArgsFor system configPath // {yolabFacterPath = facterPath;};
      };

    mkInstaller = system:
      nixpkgs.lib.nixosSystem {
        inherit system;
        modules = [
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./installer/nixos/iso-config.nix
        ];
        specialArgs = {
          inherit inputs;
          rust = rustFor.${system};
        };
      };

    baseModules = [
      disko.nixosModules.disko
      ./homelab/nixos/configuration.nix
      ./homelab/nixos/disk-config.nix
    ];

    nixosSystems =
      {
        yolab-installer = mkInstaller "x86_64-linux";
        yolab-installer-aarch64 = mkInstaller "aarch64-linux";
      }
      // lib.optionalAttrs isMachine {
        yolab = mkYolabSystem {
          system = machineSystem;
          configPath = machineConfig;
          facterPath =
            if hasFacter
            then machineFacter
            else null;
          modules = baseModules ++ lib.optional (builtins.pathExists machineHardware) machineHardware;
        };
      };

    ciSystemsFor = system: {
      yolab-ci = mkYolabSystem {
        inherit system;
        configPath = ./homelab/ci-config.toml;
        modules = baseModules;
      };
      yolab-ci-join = mkYolabSystem {
        inherit system;
        configPath = ./homelab/ci-join-config.toml;
        modules = baseModules;
      };
      yolab-ci-aarch64 = mkYolabSystem {
        system = "aarch64-linux";
        configPath = ./homelab/ci-config.toml;
        modules = baseModules;
      };
    };

    treefmtFor = lib.genAttrs systems (system:
      inputs.treefmt-nix.lib.evalModule (pkgsFor system) (
        import ./nix/treefmt.nix {inherit (rustFor.${system}) rustToolchain;}
      ));

    androidApk = import ./nix/android.nix {
      inherit pkgs rust;
      signingKey = inputs.yolab-android-key;
    };

    checksFor = system: let
      all = import ./nix/checks.nix {
        pkgs = pkgsFor system;
        treefmtEval = treefmtFor.${system};
        rust = rustFor.${system};
        nixosSystems = ciSystemsFor system;
        inherit androidApk;
      };
    in
      if system == "x86_64-linux"
      then all
      else builtins.removeAttrs all ["android-apk-is-signed-so-android-will-install-it"];

    devShellsFor = system: let
      p = pkgsFor system;
      r = rustFor.${system};
      fmt = treefmtFor.${system};
    in {
      default = p.mkShell {
        packages =
          (with p; [
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
          ++ [r.rustToolchain]
          ++ builtins.attrValues fmt.config.build.programs
          ++ [fmt.config.build.wrapper];

        shellHook = ''
          echo "yolab devshell"
        '';
      };

      desktop = p.mkShell {
        packages =
          (with p; [
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
          ++ [r.rustToolchain];

        shellHook = ''
          echo "yolab desktop shell — cd shells/desktop && cargo check"
        '';
      };
    };
  in {
    nixosConfigurations =
      {
        inherit (nixosSystems) yolab-installer yolab-installer-aarch64;
      }
      // lib.optionalAttrs isMachine {yolab = nixosSystems.yolab;};

    checks = lib.genAttrs systems checksFor;

    formatter = lib.genAttrs systems (system: treefmtFor.${system}.config.build.wrapper);

    packages = lib.genAttrs systems (system: {
      desktop-client = rustFor.${system}.crates.desktop-client.package;
      android-apk = androidApk;
    });

    apps = lib.genAttrs systems (system: {
      desktop-client = {
        type = "app";
        program = "${self.packages.${system}.desktop-client}/bin/yolab-desktop";
        meta.description = "Open the YoLab desktop window";
      };
    });

    devShells = lib.genAttrs systems devShellsFor;
  };
}
