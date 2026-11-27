{
  nixpkgs,
  system,
  serverModule,
}:
let
  defaultApp = {
    idFile = "/run/secrets/github-app-id";
    privateKeyFile = "/run/secrets/github-app-key";
    repository = "owner/project";
  };
  evaluate =
    app:
    nixpkgs.lib.nixosSystem {
      inherit system;
      modules = [
        serverModule
        {
          system.stateVersion = "25.11";
          security.acme.acceptTerms = true;
          security.acme.defaults.email = "admin@example.com";
          services.rworker = {
            enable = true;
            githubActions = {
              enable = true;
              serverName = "runner.example.com";
              authorizedKeysFile = "/run/secrets/github-relay-authorized-keys";
              inherit app;
            };
            gitlabCi = {
              enable = true;
              serverName = "gitlab-runner.example.com";
              authorizedKeysFile = "/run/secrets/gitlab-relay-authorized-keys";
            };
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
              extraMachines = [
                {
                  selector = "arch:aarch64";
                  systems = [ "aarch64-linux" ];
                }
              ];
            };
            reaper.enable = true;
            logCache.operator = "operator";
            web = {
              enable = true;
              tokenFile = "/run/secrets/web-token";
              operator = "operator";
              identityFile = "/run/secrets/web-key";
              publicKeyFile = "/run/secrets/web-key.pub";
            };
            logCache.workerUser = "rworker";
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
  evaluated = evaluate defaultApp;
  inherit (evaluated) config pkgs;
  failedAssertions =
    app:
    builtins.filter (nixpkgs.lib.hasInfix "githubActions.app") (
      map (a: a.message) (builtins.filter (a: !a.assertion) (evaluate app).config.assertions)
    );
  inherit (config.systemd) sockets services timers;
in
assert sockets.rworker-control.socketConfig.ListenStream == "/run/rworker-control.sock";
assert sockets.rworker-control.socketConfig.Accept;
assert sockets.rworker-control.socketConfig.SocketMode == "0660";
assert services."rworker-control@".serviceConfig.StandardInput == "socket";
assert services.rworker-inventory-api.serviceConfig.Group == "rworker-inventory-api";
assert services.rworker-inventory-api.serviceConfig.User == "operator";
assert
  services.rworker-inventory-api.environment.RWORKER_WEB_ORIGIN == "https://control.example.com";
assert
  (builtins.head config.services.nginx.virtualHosts."control.example.com".listen).addr == "127.0.0.1";
assert timers.rworker-reaper.timerConfig.OnUnitActiveSec == "1m";
assert services.rworker-log-cache.serviceConfig.User == "operator";
assert
  services.nixbot-autoscaler.serviceConfig.LoadCredential == [
    "rworker-web-worker-public-key:/run/secrets/web-key.pub"
  ];
assert services.rworker-job-events.serviceConfig.User == "operator";
assert services.rworker-job-events.serviceConfig.SupplementaryGroups == [ "rworker" ];
assert services."rworker-control@".environment.RWORKER_EVENT_DIRECTORY == "/var/lib/rworker-events";
assert services.rworker-log-cache.environment.RWORKER_USER == "rworker";
assert timers.rworker-log-cache.timerConfig.OnUnitInactiveSec == "30s";
assert sockets.rworker-builder.socketConfig.Accept;
assert sockets.rworker-builder.socketConfig.SocketMode == "0660";
assert sockets.rworker-builder.socketConfig.SocketGroup == "rworker-builder";
assert sockets.rworker-builder.socketConfig.MaxConnections == 8;
assert services."rworker-builder@".serviceConfig.StandardOutput == "socket";
assert services."rworker-builder@".serviceConfig.StandardError == "journal";
assert pkgs.lib.hasSuffix ''"7200" "300" "-"'' services."rworker-builder@".serviceConfig.ExecStart;
assert timers.rworker-builder-reap.timerConfig.OnUnitInactiveSec == "1m";
assert services.rworker-github-api.serviceConfig.StateDirectory == "rworker-github";
assert timers.rworker-github-reap.timerConfig.OnUnitInactiveSec == "1m";
assert services.rworker-gitlab-api.serviceConfig.StateDirectory == "rworker-gitlab";
assert timers.rworker-gitlab-reap.timerConfig.OnUnitInactiveSec == "1m";
assert services.rworker-github-token.serviceConfig.Type == "oneshot";
assert services.rworker-github-token.serviceConfig.Restart == "on-failure";
assert services.rworker-github-token.serviceConfig.RestartSec == "30s";
assert services.rworker-github-token.serviceConfig.Group == "rworker";
assert services.rworker-github-token.serviceConfig.RuntimeDirectory == "rworker-github-token";
assert services.rworker-github-token.unitConfig.StartLimitIntervalSec == 0;
assert
  services.rworker-github-token.serviceConfig.LoadCredential == [
    "app-id:/run/secrets/github-app-id"
    "private-key:/run/secrets/github-app-key"
  ];
assert timers.rworker-github-token.timerConfig.OnBootSec == "30s";
assert timers.rworker-github-token.timerConfig.OnUnitInactiveSec == "20m";
assert config.services.rworker.githubActions.app.tokenFile == "/run/rworker-github-token/token";
assert services.rworker-github-api.wants == [ "rworker-github-token.service" ];
assert pkgs.lib.hasInfix "owner/project" (
  builtins.readFile services.rworker-github-token.serviceConfig.ExecStart
);
assert failedAssertions defaultApp == [ ];
assert builtins.length (failedAssertions (defaultApp // { idFile = "/nix/store/x-id"; })) == 1;
assert builtins.length (failedAssertions (defaultApp // { repository = null; })) == 1;
assert config.services.nginx.virtualHosts."runner.example.com".forceSSL;
assert pkgs.lib.hasInfix "PermitListen 127.0.0.1:*" config.services.openssh.extraConfig;
assert config.nix.distributedBuilds;
assert (builtins.head config.nix.buildMachines).protocol == "ssh-ng";
pkgs.runCommand "rworker-server-module-check" { } ''
  test -e ${config.systemd.units."rworker-control.socket".unit}
  test -e ${config.systemd.units."rworker-control@.service".unit}
  test -e ${config.systemd.units."rworker-inventory.service".unit}
  test -e ${config.systemd.units."rworker-inventory-api.service".unit}
  test -e ${config.systemd.units."rworker-reaper.service".unit}
  test -e ${config.systemd.units."rworker-github-api.service".unit}
  test -e ${config.systemd.units."rworker-github-reap.timer".unit}
  test -e ${config.systemd.units."nixbot-autoscaler.service".unit}
  test -e ${config.systemd.units."rworker-log-cache.service".unit}
  test -e ${config.systemd.units."rworker-job-events.service".unit}
  test -e ${config.systemd.units."rworker-builder.socket".unit}
  test -e ${config.systemd.units."rworker-builder@.service".unit}
  test -e ${config.systemd.units."rworker-builder-reap.timer".unit}
  grep -q '^ssh-ng://rworker-builder@builder.example.com x86_64-linux /run/secrets/builder-key 1 1 rworker rworker dGVzdA==$' ${
    config.environment.etc."nix/machines".source
  }
  grep -q '^ssh-ng://rworker-builder@builder.example.com?remote-program=arch:aarch64 aarch64-linux /run/secrets/builder-key 1 1 - - dGVzdA==$' ${
    config.environment.etc."nix/machines".source
  }
  touch "$out"
''
