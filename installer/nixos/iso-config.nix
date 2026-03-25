{
  pkgs,
  lib,
  inputs,
  ...
}:
let
  workspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ./.;
  };
  overlay = workspace.mkPyprojectOverlay {
    sourcePreference = "wheel";
  };
  pythonSet =
    (pkgs.callPackage inputs.pyproject-nix.build.packages {
      python = pkgs.python311;
    }).overrideScope
      (
        lib.composeManyExtensions [
          inputs.pyproject-build-systems.overlays.wheel
          overlay
        ]
      );
  yolabInstaller = pythonSet.mkVirtualEnv "homelab-installer-env" workspace.deps.default;
in
{
  isoImage.makeEfiBootable = true;
  isoImage.makeUsbBootable = true;
  isoImage.squashfsCompression = "xz -Xdict-size 100%";

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
      inputs.disko.packages.${pkgs.system}.disko
      wireguard-tools
    ])
    ++ [ yolabInstaller ];
  services.getty.autologinUser = lib.mkForce "root";
  programs.bash.interactiveShellInit = ''
    if [ "$(tty)" = "/dev/tty1" ]; then
      ${yolabInstaller}/bin/yolab-installer install
    fi
  '';
  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
}
