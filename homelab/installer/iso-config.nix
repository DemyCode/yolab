{
  pkgs,
  lib,
  inputs,
  ...
}: let
  # Build backend with uv2nix (following template pattern)
  workspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ./backend;
  };

  overlay = workspace.mkPyprojectOverlay {
    sourcePreference = "wheel";
  };

  # Create Python package set with build-systems and workspace overlays
  pythonSet =
    (pkgs.callPackage inputs.pyproject-nix.build.packages {
      python = pkgs.python311;
    })
    .overrideScope
    (lib.composeManyExtensions [
      inputs.pyproject-build-systems.overlays.wheel
      overlay
    ]);

  # Create virtual environment with backend and all dependencies
  installerBackend = pythonSet.mkVirtualEnv "homelab-installer-env" workspace.deps.default;
in {
  isoImage.makeEfiBootable = true;
  isoImage.makeUsbBootable = true;
  isoImage.squashfsCompression = "xz -Xdict-size 100%";

  # Reduce ISO size
  documentation.enable = false;
  documentation.man.enable = false;
  documentation.info.enable = false;
  documentation.doc.enable = false;

  networking.networkmanager.enable = true;
  networking.wireless.enable = lib.mkForce false;

  environment.systemPackages =
    (with pkgs; [
      vim
      curl
      parted
      gptfdisk
      util-linux
    ])
    ++ [installerBackend];

  # Auto-login to TTY1 as root
  services.getty.autologinUser = lib.mkForce "root";

  # Run installer CLI on login
  programs.bash.interactiveShellInit = ''
    if [ "$(tty)" = "/dev/tty1" ]; then
      ${installerBackend}/bin/yolab-installer install
    fi
  '';

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
}
