{ pkgs
, lib
, ...
}:

let
  installerScript = pkgs.writeScriptBin "homelab-installer" ''
    #!${pkgs.python3}/bin/python3
    ${builtins.readFile ./backend/service.py}
  '';

in
{
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
    python3
    installerScript
    jq
  ];

  systemd.services.homelab-installer = {
    description = "Homelab Installer Service";
    after = [ "network.target" ];
    wantedBy = [ "multi-user.target" ];
    serviceConfig = {
      Type = "simple";
      ExecStart = "${installerScript}/bin/homelab-installer";
      Restart = "always";
      RestartSec = "10s";
    };
  };

  services.getty.autologinUser = "nixos";

  programs.bash.interactiveShellInit = ''
        cat <<EOF

        ╔══════════════════════════════════════════════════════════╗
        ║                                                          ║
        ║           Homelab NixOS Installer                        ║
        ║                                                          ║
        ╚══════════════════════════════════════════════════════════╝

        Installer service is running on http://0.0.0.0:8000

        Web UI: Open http://localhost:8000 in a browser

        Or use CLI:
          curl http://localhost:8000/detect | jq
          curl -X POST http://localhost:8000/install \\
            -H "Content-Type: application/json" \\
            -d '{"disk": "/dev/sda", "hostname": "homelab", "root_ssh_key": "ssh-ed25519 ..."}'

        Useful commands:
          lsblk               - List disks
          ip a                - Show network interfaces
          systemctl status homelab-installer - Check installer status

    EOF
  '';

  nix.settings.experimental-features = [
    "nix-command"
    "flakes"
  ];
}
