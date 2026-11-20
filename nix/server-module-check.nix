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
          builderGateway = {
            enable = true;
            authorizedKeysFile = "/run/secrets/builder-authorized-keys";
          };
          builderClient = {
            enable = true;
            hostName = "builder.example.com";
            identityFile = "/run/secrets/builder-key";
            publicHostKey = "dGVzdA==";
            systems = [ "x86_64-linux" ];
            mandatoryFeatures = [ "rworker" ];
          };
          reaper.enable = true;
          logCache.operator = "operator";
          logCache.workerUser = "agent";
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
assert services.rworker-job-events.serviceConfig.User == "operator";
assert services.rworker-job-events.serviceConfig.SupplementaryGroups == [ "rworker" ];
assert services."rworker-control@".environment.RWORKER_EVENT_DIRECTORY == "/var/lib/rworker-events";
assert services.rworker-log-cache.environment.RWORKER_USER == "agent";
assert timers.rworker-log-cache.timerConfig.OnUnitInactiveSec == "30s";
assert sockets.rworker-builder.socketConfig.Accept;
assert sockets.rworker-builder.socketConfig.SocketMode == "0660";
assert sockets.rworker-builder.socketConfig.SocketGroup == "rworker-builder";
assert sockets.rworker-builder.socketConfig.MaxConnections == 8;
assert services."rworker-builder@".serviceConfig.StandardOutput == "socket";
assert services."rworker-builder@".serviceConfig.StandardError == "journal";
assert pkgs.lib.hasSuffix ''"7200" "300" "-"'' services."rworker-builder@".serviceConfig.ExecStart;
assert timers.rworker-builder-reap.timerConfig.OnUnitInactiveSec == "1m";
assert config.nix.distributedBuilds;
assert (builtins.head config.nix.buildMachines).protocol == "ssh-ng";
pkgs.runCommand "rworker-server-module-check" { } ''
  test -e ${config.systemd.units."rworker-control.socket".unit}
  test -e ${config.systemd.units."rworker-control@.service".unit}
  test -e ${config.systemd.units."rworker-inventory.service".unit}
  test -e ${config.systemd.units."rworker-inventory-api.service".unit}
  test -e ${config.systemd.units."rworker-reaper.service".unit}
  test -e ${config.systemd.units."nixbot-autoscaler.service".unit}
  test -e ${config.systemd.units."rworker-log-cache.service".unit}
  test -e ${config.systemd.units."rworker-job-events.service".unit}
  test -e ${config.systemd.units."rworker-builder.socket".unit}
  test -e ${config.systemd.units."rworker-builder@.service".unit}
  test -e ${config.systemd.units."rworker-builder-reap.timer".unit}
  grep -q '^ssh-ng://rworker-builder@builder.example.com x86_64-linux /run/secrets/builder-key 1 1 rworker rworker dGVzdA==$' ${
    config.environment.etc."nix/machines".source
  }
  touch "$out"
''
