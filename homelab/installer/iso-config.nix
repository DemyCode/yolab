{
  pkgs,
  lib,
  inputs,
  ...
}: let
  # Build backend with uv2nix
  workspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ./backend;
  };

  overlay = workspace.mkPyprojectOverlay {
    sourcePreference = "wheel";
  };

  # Create Python set with pyproject-nix build system
  pythonSet =
    (pkgs.callPackage inputs.pyproject-nix.build.packages {
      python = pkgs.python311;
    })
    .overrideScope
    overlay;

  # Create virtual environment with backend and all dependencies
  installerBackend = pythonSet.mkVirtualEnv "homelab-installer-env" workspace.deps.default;
  # Build the React frontend
  frontend = pkgs.buildNpmPackage {
    pname = "homelab-installer-frontend";
    version = "1.0.0";
    src = ./frontend;
    npmDepsHash = lib.fakeHash;

    buildPhase = ''
      npm run build
    '';

    installPhase = ''
      mkdir -p $out
      cp -r dist/* $out/
    '';
  };
in {
  isoImage.makeEfiBootable = true;
  isoImage.makeUsbBootable = true;

  networking.networkmanager.enable = true;
  networking.wireless.enable = lib.mkForce false;

  environment.systemPackages = with pkgs; [
    vim
    curl
    git
    parted
    gptfdisk
    util-linux
    iproute2
    jq
    nodejs
  ];

  systemd.services.homelab-installer-backend = {
    description = "Homelab Installer Backend API";
    after = ["network.target"];
    wantedBy = ["multi-user.target"];
    path = with pkgs; [
      git
      parted
      gptfdisk
      util-linux
      nixos-install-tools
    ];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${installerBackend}/bin/uvicorn backend.main:app --host 0.0.0.0 --port 8000";
      Restart = "always";
      RestartSec = "5s";
    };
  };

  systemd.services.homelab-installer-frontend = {
    description = "Homelab Installer Frontend";
    after = ["network.target"];
    wantedBy = ["multi-user.target"];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${pkgs.python311}/bin/python -m http.server 3000 --directory ${frontend}";
      Restart = "always";
      RestartSec = "5s";
    };
  };

  services.xserver = {
    enable = true;
    displayManager = {
      lightdm = {
        enable = true;
        autoLogin = {
          enable = true;
          user = "nixos";
        };
      };
    };
    desktopManager.xterm.enable = false;
    windowManager.openbox.enable = true;
  };

  services.getty.autologinUser = lib.mkForce null;

  environment.etc."xdg/openbox/autostart".text = ''
    #!/bin/sh
    sleep 3
    ${pkgs.firefox}/bin/firefox --kiosk --private-window http://localhost:3000/ &
  '';

  users.users.nixos.packages = with pkgs; [
    firefox
    openbox
  ];

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
}
