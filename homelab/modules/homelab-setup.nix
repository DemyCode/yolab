{ config, pkgs, lib, ... }:

let
  # Load machine-specific configuration from TOML
  configPath = ../config.toml;
  homelabConfig = if builtins.pathExists configPath
    then builtins.fromTOML (builtins.readFile configPath)
    else {};

  # Extract homelab-setup configuration
  setupCfg = homelabConfig.setup or {};
  setupEnabled = setupCfg.enabled or false;
  setupPort = setupCfg.port or 5001;
  registrationApiUrl = setupCfg.registration_api_url or "";

  # Simple Python service for remote configuration
  setupService = pkgs.writeScriptBin "homelab-setup-service" ''
    #!${pkgs.python3}/bin/python3
    import http.server
    import json
    import os
    import subprocess
    import socketserver
    from urllib.parse import urlparse, parse_qs

    PORT = ${toString setupPort}
    CONFIG_FILE = "/etc/homelab/config.toml"
    REGISTRATION_API = "${registrationApiUrl}"

    class HomelabSetupHandler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            if self.path == "/health":
                self.send_response(200)
                self.send_header("Content-type", "application/json")
                self.end_headers()
                self.wfile.write(json.dumps({"status": "healthy", "service": "homelab-setup"}).encode())
            elif self.path == "/status":
                self.send_response(200)
                self.send_header("Content-type", "application/json")
                self.end_headers()
                status = {
                    "hostname": os.uname().nodename,
                    "config_exists": os.path.exists(CONFIG_FILE),
                    "registration_api": REGISTRATION_API,
                }
                self.wfile.write(json.dumps(status).encode())
            else:
                self.send_response(404)
                self.end_headers()

        def do_POST(self):
            if self.path == "/configure":
                content_length = int(self.headers['Content-Length'])
                post_data = self.rfile.read(content_length)

                try:
                    config = json.loads(post_data)
                    # TODO: Validate and apply configuration
                    # This is a placeholder for the actual configuration logic

                    self.send_response(200)
                    self.send_header("Content-type", "application/json")
                    self.end_headers()
                    self.wfile.write(json.dumps({"status": "success", "message": "Configuration received"}).encode())
                except Exception as e:
                    self.send_response(500)
                    self.send_header("Content-type", "application/json")
                    self.end_headers()
                    self.wfile.write(json.dumps({"status": "error", "message": str(e)}).encode())
            else:
                self.send_response(404)
                self.end_headers()

    with socketserver.TCPServer(("", PORT), HomelabSetupHandler) as httpd:
        print(f"Homelab Setup Service listening on port {PORT}")
        httpd.serve_forever()
  '';

in {
  config = lib.mkIf setupEnabled {
    # Create homelab config directory
    systemd.tmpfiles.rules = [
      "d /etc/homelab 0755 root root -"
    ];

    # Homelab setup service
    systemd.services.homelab-setup = {
      description = "Homelab Setup and Configuration Service";
      after = [ "network.target" ];
      wantedBy = [ "multi-user.target" ];
      serviceConfig = {
        Type = "simple";
        ExecStart = "${setupService}/bin/homelab-setup-service";
        Restart = "always";
        RestartSec = "10s";
        User = "root";
      };
    };

    # Note: Firewall is disabled in configuration.nix, so no port opening needed
  };
}
