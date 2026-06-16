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

    packages.x86_64-linux = let
      builds = import ./homelab/builds.nix {inherit pkgs inputs;};
    in {
      iso = self.nixosConfigurations.yolab-installer.config.system.build.isoImage;
      homelab-ui = builds.clientUi;
      homelab-api = builds.localApiEnv;
    };

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
          # Rust (version from rust-toolchain.toml)
          rustToolchain
          pkg-config
          openssl
          uv
          # Node.js
          nodejs
          # Runner
          pre-commit
        ];
      };
  };
}
