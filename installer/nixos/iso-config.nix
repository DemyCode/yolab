{
  pkgs,
  lib,
  inputs,
  ...
}:
let
  workspace = inputs.uv2nix.lib.workspace.loadWorkspace {
    workspaceRoot = ./backend;
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

  installerFrontend = pkgs.buildNpmPackage {
    pname = "installer-frontend";
    version = "0.1.0";
    src = ./frontend;
    npmDepsHash = "sha256-uygBGqWBRliZIr0c/atYSKh2Gn9d3716xDVB99OrsVY=";
    installPhase = "cp -r dist $out";
  };
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
  networking.firewall.allowedTCPPorts = [ 80 443 ];

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

  # Frontend path available to both the web-UI service and the interactive
  # installer (which writes the Caddy vhost with a file_server pointing here).
  environment.variables.INSTALLER_FRONTEND_PATH = "${installerFrontend}";

  # Caddy is configured dynamically by the installer after WireGuard is up.
  # interactive.py writes /etc/caddy/installer.caddy and reloads the service.
  services.caddy = {
    enable = true;
    configFile = pkgs.writeText "Caddyfile" ''
      import /etc/caddy/*.caddy
    '';
  };

  systemd.services.yolab-installer-ui = {
    description = "YoLab Installer Web UI";
    wantedBy = [ "multi-user.target" ];
    after = [ "network.target" ];
    environment.PATH = lib.mkForce "/run/current-system/sw/bin:/nix/var/nix/profiles/default/bin";
    serviceConfig = {
      ExecStart = "${yolabInstaller}/bin/yolab-installer serve";
      Restart = "on-failure";
      RestartSec = "2s";
    };
  };

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
