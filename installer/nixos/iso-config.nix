{
  pkgs,
  lib,
  inputs,
  rust,
  ...
}: let
  # The same derivation `installer-tests` runs its tests against. This used to
  # be a local `inputs.crane.mkLib pkgs` — nixpkgs' rustc rather than the one
  # rust-toolchain.toml pins, and missing the pkg-config the tests pass — so
  # the binary that shipped on the ISO was built by a different compiler than
  # the binary the tests exercised.
  yolabInstaller = rust.crates.installer.package;
in {
  isoImage.makeEfiBootable = true;
  isoImage.makeUsbBootable = true;
  # THE ISO JOB'S LONG POLE IS THIS LINE, not the packages it ships.
  #
  # `xz -Xdict-size 100%` is the nixpkgs default and the slowest setting on
  # offer: xz at a full-image dictionary is effectively single-threaded through
  # the final block and runs at single-digit MB/s, so most of the ISO job's wall
  # clock is one `mksquashfs` compressing a multi-gigabyte filesystem — work no
  # amount of CI parallelism can divide, because it is one process in one
  # derivation.
  #
  # zstd at a high level is the trade this wants: dramatically faster to produce
  # and to decompress, for an image somewhere in the region of 15-30% larger.
  # That is a good deal here — the ISO is written to a USB stick once, while this
  # is rebuilt on every push to main.
  #
  # If ISO SIZE ever matters more than build time (metered hosting, slow
  # downloads for the people installing it), put the old value back — it is a
  # one-line revert and changes nothing else:
  #
  #   isoImage.squashfsCompression = "xz -Xdict-size 100%";
  #
  # Kernel support is not a risk: CONFIG_SQUASHFS_ZSTD is on in the NixOS kernels
  # this ISO builds against, and `squashfsCompression` is passed through to
  # mksquashfs verbatim by the upstream iso-image module.
  isoImage.squashfsCompression = "zstd -Xcompression-level 19";

  documentation.enable = false;
  documentation.man.enable = false;
  documentation.info.enable = false;
  documentation.doc.enable = false;

  networking.networkmanager.enable = true;
  networking.wireless.enable = lib.mkForce false;
  # Always inject these resolvers so DNS works immediately on boot,
  # even before DHCP delivers the router's DNS. NM merges these in.
  networking.networkmanager.insertNameservers = [
    "9.9.9.9"
    "1.1.1.1"
    "8.8.8.8"
  ];

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

  nix.settings.substituters = [
    "https://cache.nixos.org"
    "https://cache.demycode.ovh/yolab"
  ];
  nix.settings.trusted-public-keys = [
    "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="
    "yolab:3CIkfuGsBgTSWSAZJ2FCbVXjLG1RwNJvvGS1MAtQCmQ="
  ];

  # Auto-login as root and immediately launch the TUI installer on tty1.
  services.getty.autologinUser = lib.mkForce "root";
  programs.bash.interactiveShellInit = ''
    if [ "$(tty)" = "/dev/tty1" ]; then
      exec ${yolabInstaller}/bin/yolab-installer
    fi
  '';
}
