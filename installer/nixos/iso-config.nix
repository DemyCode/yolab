{
  pkgs,
  lib,
  inputs,
  rust,
  ...
}: let
  yolabInstaller = rust.crates.installer.package;
in {
  isoImage.makeEfiBootable = true;
  isoImage.makeUsbBootable = true;
  isoImage.squashfsCompression = "zstd -Xcompression-level 19";

  documentation.enable = false;
  documentation.man.enable = false;
  documentation.info.enable = false;
  documentation.doc.enable = false;

  networking.networkmanager.enable = true;
  networking.wireless.enable = lib.mkForce false;
  networking.networkmanager.insertNameservers = [
    "9.9.9.9"
    "1.1.1.1"
    "8.8.8.8"
  ];

  environment.systemPackages = with pkgs; [
    vim
    curl
    git
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

  nix.settings.substituters = [
    "https://cache.nixos.org"
    "https://cache.yolab.io/yolab"
  ];
  nix.settings.trusted-public-keys = [
    "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
    "yolab:3CIkfuGsBgTSWSAZJ2FCbVXjLG1RwNJvvGS1MAtQCmQ="
  ];

  services.getty.autologinUser = lib.mkForce "root";
  programs.bash.interactiveShellInit = ''
    if [ "$(tty)" = "/dev/tty1" ]; then
      exec ${yolabInstaller}/bin/yolab-installer
    fi
  '';
}
