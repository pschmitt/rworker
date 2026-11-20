{
  nixpkgs,
  system,
  serverModule,
}:
let
  evaluated = nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [
      serverModule
      {
        system.stateVersion = "25.11";
        services.rworker = {
          enable = true;
          reaper.enable = true;
          logCache.operator = "operator";
          api.https = {
            serverName = "control.example.com";
            listenAddress = "127.0.0.1";
            certificateFile = "/run/secrets/server.pem";
            certificateKeyFile = "/run/secrets/server-key.pem";
            clientCAFile = "/run/secrets/client-ca.pem";
          };
          settingsFile = "/run/secrets/control.json";
          autoscaler = {
            enable = true;
            settingsFile = "/run/secrets/autoscaler.json";
          };
        };
      }
    ];
  };
  inherit (evaluated) config pkgs;
  inherit (config.systemd) sockets services timers;
in
assert sockets.rworker-control.socketConfig.ListenStream == "/run/rworker-control.sock";
assert sockets.rworker-control.socketConfig.Accept;
assert sockets.rworker-control.socketConfig.SocketMode == "0660";
assert services."rworker-control@".serviceConfig.StandardInput == "socket";
assert services.rworker-inventory-api.serviceConfig.Group == "rworker-inventory-api";
assert
  (builtins.head config.services.nginx.virtualHosts."control.example.com".listen).addr == "127.0.0.1";
assert timers.rworker-reaper.timerConfig.OnUnitActiveSec == "1m";
assert services.rworker-log-cache.serviceConfig.User == "operator";
assert timers.rworker-log-cache.timerConfig.OnUnitInactiveSec == "30s";
pkgs.runCommand "rworker-server-module-check" { } ''
  test -e ${config.systemd.units."rworker-control.socket".unit}
  test -e ${config.systemd.units."rworker-control@.service".unit}
  test -e ${config.systemd.units."rworker-inventory.service".unit}
  test -e ${config.systemd.units."rworker-inventory-api.service".unit}
  test -e ${config.systemd.units."rworker-reaper.service".unit}
  test -e ${config.systemd.units."nixbot-autoscaler.service".unit}
  test -e ${config.systemd.units."rworker-log-cache.service".unit}
  touch "$out"
''
