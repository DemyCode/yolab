{ pkgs, lib, ... }:

let
  configPath = ../config.toml;
  homelabConfig =
    if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else throw "config.toml not found! Please create it from config.toml.example";

  frpcConfig = homelabConfig.frpc or (throw "[frpc] section missing in config.toml");
  frpcEnabled = frpcConfig.enabled or (throw "[frpc] enabled is required in config.toml");
  serverAddr = frpcConfig.server_addr or (throw "[frpc] server_addr is required in config.toml");
  serverPort = frpcConfig.server_port or (throw "[frpc] server_port is required in config.toml");
  accountToken = frpcConfig.account_token or (throw "[frpc] account_token is required in config.toml");
  services = frpcConfig.services or (throw "[frpc] services array is required in config.toml");

  generateFrpcConfig = service:
    let
      serviceName = service.name or (throw "Service missing 'name' field in [frpc.services]");
      serviceType = service.type or (throw "Service '${serviceName}' missing 'type' field");
      localPort = toString (service.local_port or (throw "Service '${serviceName}' missing 'local_port' field"));
      remotePort = toString (service.remote_port or (throw "Service '${serviceName}' missing 'remote_port' field"));
    in
    ''
      [common]
      server_addr = ${serverAddr}
      server_port = ${toString serverPort}
      user = ${serviceName}
      meta_account_token = ${accountToken}

      [${serviceName}]
      type = ${serviceType}
      local_ip = 127.0.0.1
      local_port = ${localPort}
      remote_port = ${remotePort}
    '';

  createFrpcService = service:
    let
      serviceName = service.name;
      configFile = pkgs.writeText "frpc-${serviceName}.ini" (generateFrpcConfig service);
    in
    {
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

in
{
  config = lib.mkIf frpcEnabled {
    users.users.frpc = {
      isSystemUser = true;
      group = "frpc";
      description = "FRP Client user";
    };

    users.groups.frpc = { };

    systemd.services = builtins.listToAttrs (map createFrpcService services);
  };
}
