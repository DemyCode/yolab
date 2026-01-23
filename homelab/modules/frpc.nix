{ config, pkgs, lib, ... }:

let
  # Load machine-specific configuration from TOML
  configPath = ../config.toml;
  homelabConfig = if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else {};

  # Extract frpc configuration
  frpcConfig = homelabConfig.frpc or {};
  frpcEnabled = frpcConfig.enabled or false;
  serverAddr = frpcConfig.server_addr or "";
  accountToken = frpcConfig.account_token or "";
  services = frpcConfig.services or [];

  # Validate configuration
  _ = if frpcEnabled && serverAddr == ""
      then throw "FRPC enabled but server_addr is not set in config.toml!"
      else if frpcEnabled && accountToken == ""
      then throw "FRPC enabled but account_token is not set in config.toml!"
      else if frpcEnabled && services == []
      then throw "FRPC enabled but no services configured in config.toml!"
      else null;

  # Generate frpc config for a single service
  generateFrpcConfig = service:
    let
      serviceName = service.name;
      serviceType = service.type or "tcp";
      localPort = toString service.local_port;
      remotePort = toString service.remote_port;
    in ''
      [common]
      server_addr = ${frpcConfig.server_addr or ""}
      server_port = ${toString (frpcConfig.server_port or 7000)}
      user = ${serviceName}
      meta_account_token = ${accountToken}

      [${serviceName}]
      type = ${serviceType}
      local_ip = 127.0.0.1
      local_port = ${localPort}
      remote_port = ${remotePort}
    '';

  # Create a systemd service for each frpc service
  createFrpcService = service:
    let
      serviceName = service.name;
      configFile = pkgs.writeText "frpc-${serviceName}.ini" (generateFrpcConfig service);
    in {
      name = "frpc-${serviceName}";
      value = {
        description = "FRP Client for ${serviceName} - ${service.description or serviceName}";
        after = [ "network.target" ];
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          Type = "simple";
          ExecStart = "${pkgs.frp}/bin/frpc -c ${configFile}";
          Restart = "always";
          RestartSec = "10s";
          User = "frpc";
          Group = "frpc";
        };
      };
    };

in {
  config = lib.mkIf frpcEnabled {
    # Create frpc user
    users.users.frpc = {
      isSystemUser = true;
      group = "frpc";
      description = "FRP Client user";
    };

    users.groups.frpc = {};

    # Create systemd services for each configured service
    systemd.services = builtins.listToAttrs (map createFrpcService services);
  };
}
