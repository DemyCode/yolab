{
  description = "YoLab - IPv6 Tunneling Platform with FRP";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    disko.url = "github:nix-community/disko";
    disko.inputs.nixpkgs.follows = "nixpkgs";
  };

  outputs = { self, nixpkgs, disko, ... }: {
    # Server Configurations (Cloud Deployment)
    nixosConfigurations = {
      # Legacy separate servers
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

      # All-in-one server (FRP + Services on same machine)
      yolab-server = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          disko.nixosModules.disko
          ./deployment/nixos/all-in-one.nix
        ];
      };

      # Client Configuration (Homelab)
      yolab-client = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          disko.nixosModules.disko
          ./homelab/nixos/configuration.nix
          ./homelab/nixos/disk-config.nix
          ./homelab/nixos/modules/frpc.nix
          ./homelab/nixos/modules/client-ui.nix
        ];
      };

      # Installer ISO
      yolab-installer = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          "${nixpkgs}/nixos/modules/installer/cd-dvd/installation-cd-minimal.nix"
          ./homelab/installer/iso-config.nix
        ];
      };
    };

    # Packages
    packages.x86_64-linux = {
      iso = self.nixosConfigurations.yolab-installer.config.system.build.isoImage;
    };
    
  };
}
