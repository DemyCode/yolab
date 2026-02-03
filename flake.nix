{
  description = "YoLab Packages";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs =
    {
      self,
      nixpkgs,
      disko,
      ...
    }:
    {
      nixosConfigurations = {
        frps-server = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            disko.nixosModules.disko
            ./deployment/nixos/frps-server.nix
          ];
        };
        services-stack = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            disko.nixosModules.disko
            ./deployment/nixos/services-stack.nix
          ];
        };
        yolab-server = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            disko.nixosModules.disko
            ./deployment/nixos/all-in-one.nix
          ];
        };
        yolab = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            disko.nixosModules.disko
            ./homelab/nixos/configuration.nix
            ./homelab/nixos/disk-config.nix
            ./homelab/nixos/modules/frpc.nix
            ./homelab/nixos/modules/client-ui.nix
          ];
        };
        yolab-installer = nixpkgs.lib.nixosSystem {
          system = "x86_64-linux";
          modules = [
            "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
            ./homelab/installer/iso-config.nix
          ];
        };
      };
      packages.x86_64-linux = {
        iso = self.nixosConfigurations.yolab-installer.config.system.build.isoImage;
      };
    };
}
