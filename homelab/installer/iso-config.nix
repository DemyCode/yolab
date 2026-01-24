{
  pkgs,
  lib,
  ...
}: let
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

  # Backend directory with all Python files
  backend = pkgs.runCommand "homelab-installer-backend" {} ''
    mkdir -p $out
    cp ${./backend/main.py} $out/main.py
    cp ${./backend/functions.py} $out/functions.py
    cp ${./backend/cli.py} $out/cli.py
  '';
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
    python311
    python311Packages.fastapi
    python311Packages.uvicorn
    python311Packages.httpx
    python311Packages.pydantic
    python311Packages.pydantic-settings
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
      python311
      python311Packages.fastapi
      python311Packages.uvicorn
      python311Packages.httpx
      python311Packages.pydantic
      python311Packages.pydantic-settings
    ];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${pkgs.python311}/bin/python -m uvicorn main:app --host 0.0.0.0 --port 8000";
      WorkingDirectory = "${backend}";
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
