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
assert timers.rworker-reaper.timerConfig.OnUnitActiveSec == "1m";
pkgs.runCommand "rworker-server-module-check" { } ''
  test -e ${config.systemd.units."rworker-control.socket".unit}
  test -e ${config.systemd.units."rworker-control@.service".unit}
  test -e ${config.systemd.units."rworker-reaper.service".unit}
  test -e ${config.systemd.units."nixbot-autoscaler.service".unit}
  touch "$out"
''
