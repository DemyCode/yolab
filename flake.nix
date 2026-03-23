{
  description = "YoLab Packages";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";
    nixos-wsl.url = "github:nix-community/NixOS-WSL";
    nixos-wsl.inputs.nixpkgs.follows = "nixpkgs";
    nix-darwin.url = "github:LnL7/nix-darwin";
    nix-darwin.inputs.nixpkgs.follows = "nixpkgs";
    pyproject-nix.url = "github:pyproject-nix/pyproject.nix";
    pyproject-nix.inputs.nixpkgs.follows = "nixpkgs";
    uv2nix.url = "github:pyproject-nix/uv2nix";
    uv2nix.inputs.pyproject-nix.follows = "pyproject-nix";
    uv2nix.inputs.nixpkgs.follows = "nixpkgs";
    pyproject-build-systems.url = "github:pyproject-nix/build-system-pkgs";
    pyproject-build-systems.inputs.pyproject-nix.follows = "pyproject-nix";
    pyproject-build-systems.inputs.uv2nix.follows = "uv2nix";
    pyproject-build-systems.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      disko,
      nixos-wsl,
      nix-darwin,
      ...
    }@inputs:
    let
      mkDarwinSystem = system: nix-darwin.lib.darwinSystem {
        inherit system;
        modules = [ ./homelab/darwin/configuration.nix ];
        specialArgs = { inherit inputs; };
      };
    in
    {
      nixosConfigurations = {
        wireguard-server = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            disko.nixosModules.disko
            ./deployment/nixos/wireguard-server.nix
          ];
          specialArgs = { inherit inputs; };
        };
        services-stack = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            disko.nixosModules.disko
            ./deployment/nixos/services-stack.nix
          ];
          specialArgs = { inherit inputs; };
        };
        yolab = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            disko.nixosModules.disko
            ./homelab/nixos/configuration.nix
            ./homelab/nixos/disk-config.nix
          ];
          specialArgs = { inherit inputs; };
        };
        yolab-wsl = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            nixos-wsl.nixosModules.default
            ./homelab/nixos/wsl.nix
          ];
          specialArgs = { inherit inputs; };
        };
        yolab-installer = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
            ./installer/iso-config.nix
          ];
          specialArgs = { inherit inputs; };
        };
      };

      darwinConfigurations = {
        # Apple Silicon (M1/M2/M3/M4)
        "yolab-mac" = mkDarwinSystem "aarch64-darwin";
        # Intel Mac
        "yolab-mac-x86" = mkDarwinSystem "x86_64-darwin";
      };

      packages.x86_64-linux = {
        iso = self.nixosConfigurations.yolab-installer.config.system.build.isoImage;
      };
    };
}
