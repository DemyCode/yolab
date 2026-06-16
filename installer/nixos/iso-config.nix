{
  pkgs,
  lib,
  inputs,
  ...
}: let
  craneLib = inputs.crane.mkLib pkgs;
  yolabInstaller = craneLib.buildPackage {
    src = craneLib.cleanCargoSource ./backend-rs;
  };
in {
  isoImage.makeEfiBootable = true;
  isoImage.makeUsbBootable = true;
  isoImage.squashfsCompression = "xz -Xdict-size 100%";

  documentation.enable = false;
  documentation.man.enable = false;
  documentation.info.enable = false;
  documentation.doc.enable = false;

  networking.networkmanager.enable = true;
  networking.wireless.enable = lib.mkForce false;
  # Always inject these resolvers so DNS works immediately on boot,
  # even before DHCP delivers the router's DNS. NM merges these in.
  networking.networkmanager.insertNameservers = ["9.9.9.9" "1.1.1.1" "8.8.8.8"];

  environment.systemPackages = with pkgs; [
    vim
    curl
    git
    rsync
    parted
    gptfdisk
    util-linux
    openssh
    openssl
    wireguard-tools
    inputs.disko.packages.${pkgs.system}.disko
    yolabInstaller
  ];

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];

  # Auto-login as root and immediately launch the TUI installer on tty1.
  services.getty.autologinUser = lib.mkForce "root";
  programs.bash.interactiveShellInit = ''
    if [ "$(tty)" = "/dev/tty1" ]; then
      exec ${yolabInstaller}/bin/yolab-installer
    fi
  '';
}
