{
  description = "Yolab";

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

    mkDarwinSystem = system:
      nix-darwin.lib.darwinSystem {
        inherit system;
        modules = [./homelab/darwin/configuration.nix];
        specialArgs = {inherit inputs;};
      };
  in {
    nixosConfigurations = {
      yolab = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          disko.nixosModules.disko
          ./homelab/nixos/configuration.nix
          ./homelab/nixos/disk-config.nix
        ];
        specialArgs = {inherit inputs;};
      };
      yolab-wsl = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          nixos-wsl.nixosModules.default
          ./homelab/nixos/wsl.nix
        ];
        specialArgs = {inherit inputs;};
      };
      yolab-installer = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./installer/nixos/iso-config.nix
        ];
        specialArgs = {inherit inputs;};
      };
    };

    darwinConfigurations = {
      "yolab-mac" = mkDarwinSystem "aarch64-darwin";
      "yolab-mac-x86" = mkDarwinSystem "x86_64-darwin";
    };

    # Every check lives in checks.nix as a derivation, so the same command runs
    # them on a laptop and on a runner. `nix flake check` builds all of them.
    checks.x86_64-linux = import ./checks.nix {inherit pkgs inputs;};

    packages.x86_64-linux = let
      builds = import ./homelab/builds.nix {inherit pkgs inputs;};
      checks = self.checks.x86_64-linux;
    in {
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
      }
      // lib.mapAttrs (_: drv: {
        type = "app";
        program = toString (pkgs.writeShellScript "check" "echo ${drv}");
      })
      self.checks.x86_64-linux;

    devShells.x86_64-linux.default = let
      pkgsWithOverlay = pkgs.extend inputs.rust-overlay.overlays.default;
      rustToolchain =
        pkgsWithOverlay.rust-bin.fromRustupToolchainFile
        ./homelab/local-api/rust-toolchain.toml;
    in
      pkgs.mkShell {
        packages = with pkgs; [
          # Nix
          alejandra
          statix
          deadnix
          # Shell / Docker
          shellcheck
          hadolint
          # Apps are Helm charts — needed to lint/template them locally.
          kubernetes-helm
          # Rust (version from rust-toolchain.toml)
          rustToolchain
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
        ];

        shellHook = ''
          echo "yolab devshell — 'nix run .#ci' runs every check exactly as CI does"
        '';
      };
  };
}
