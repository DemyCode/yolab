{
  description = "YoLab Client NixOS Configuration";

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
    }@inputs:
    let
      pkgs = import nixpkgs { system = "x86_64-linux"; };
    in
    {
      nixosConfigurations.yolab = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          disko.nixosModules.disko
          ./nixos/configuration.nix
          ./nixos/disk-config.nix
          ./nixos/modules/frpc.nix
          ./nixos/modules/client-ui.nix
        ];
        specialArgs = { inherit inputs; };
      };

      nixosConfigurations.installer = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./installer/iso-config.nix
        ];
        specialArgs = { inherit inputs; };
      };

      packages.x86_64-linux.iso = self.nixosConfigurations.installer.config.system.build.isoImage;
    };
}
