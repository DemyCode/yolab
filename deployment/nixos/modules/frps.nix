{
  config,
  pkgs,
  lib,
  ...
}:

with lib;

let
  cfg = config.services.yolab-frps;
in
{
  options.services.yolab-frps = {
    enable = mkEnableOption "YoLab FRP Server";

    authPluginAddr = mkOption {
      type = types.str;
      description = "Auth plugin address";
    };

    bindPort = mkOption {
      type = types.int;
      description = "FRP server bind port";
    };
  };

  config = mkIf cfg.enable {
    users.users.frps = {
      isSystemUser = true;
      group = "frps";
      home = "/var/lib/frps";
      createHome = true;
    };

    users.groups.frps = { };

    environment.systemPackages = [ pkgs.frp ];

    environment.etc."frps/frps.ini" = {
      text = ''
        [common]
        bind_addr = 0.0.0.0
        bind_port = ${toString cfg.bindPort}

        [plugin.user_auth]
        addr = ${cfg.authPluginAddr}
        path = /handler
        ops = NewProxy
      '';
      mode = "0644";
    };

    systemd.services.frps = {
      description = "FRP Server";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];

      serviceConfig = {
        Type = "simple";
        User = "frps";
        Group = "frps";
        Restart = "on-failure";
        RestartSec = "5s";
        ExecStart = "${pkgs.frp}/bin/frps -c /etc/frps/frps.ini";
      };
    };
  };
}
