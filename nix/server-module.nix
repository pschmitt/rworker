{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  cfg = config.services.rworker;
  inventoryDirectory = "/run/rworker-inventory";
  socket = "/run/rworker-control.sock";
  apiSocket = "/run/rworker-inventory-api/http.sock";
  apiGroup = if cfg.api.https == null then cfg.group else "rworker-inventory-api";
  runtimeTools = [
    pkgs.coreutils
    pkgs.curl
    pkgs.openssh
    pkgs.openssl
    pkgs.openstackclient
  ];
  command = args: utils.escapeSystemdExecArgs ([ (lib.getExe cfg.package) ] ++ args);
  client = pkgs.symlinkJoin {
    name = "rworker-local-client";
    paths = [ cfg.package ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = ''
      wrapProgram "$out/bin/rworker" \
        --set RWORKER_CONTROL_SOCKET ${socket} \
        --set RWORKER_CONTROL_SSH_COMPAT 1
    '';
  };
in
{
  options.services.rworker = {
    enable = lib.mkEnableOption "the rworker control socket and local client";
    api.https = lib.mkOption {
      default = null;
      description = "Serve the read API through nginx with mandatory client certificates on a private address.";
      type = lib.types.nullOr (
        lib.types.submodule {
          options = {
            serverName = lib.mkOption { type = lib.types.str; };
            listenAddress = lib.mkOption { type = lib.types.str; };
            certificateFile = lib.mkOption { type = lib.types.str; };
            certificateKeyFile = lib.mkOption { type = lib.types.str; };
            clientCAFile = lib.mkOption { type = lib.types.str; };
          };
        }
      );
    };
    reaper.enable = lib.mkEnableOption "periodic reaping of expired ad-hoc workers";
    reaper.dryRun = lib.mkEnableOption "reporting reaper decisions without changing workers";
    autoscaler = {
      enable = lib.mkEnableOption "nixbot build-pool autoscaling";
      settingsFile = lib.mkOption {
        type = lib.types.str;
        description = "Runtime JSON settings path for the autoscaler.";
      };
      user = lib.mkOption {
        type = lib.types.str;
        default = "nixbot";
        description = "Existing account running the autoscaler.";
      };
      group = lib.mkOption {
        type = lib.types.str;
        default = "nixbot";
        description = "Existing group running the autoscaler.";
      };
    };
    package = lib.mkOption {
      type = lib.types.package;
      description = "The rworker package providing both client and server.";
    };
    settingsFile = lib.mkOption {
      type = lib.types.str;
      example = "/run/secrets/rworker-settings.json";
      description = "Runtime JSON settings path. The service user must be able to read it and the referenced credential files.";
    };
    user = lib.mkOption {
      type = lib.types.str;
      default = "rworker";
      description = "Existing account under which control requests run.";
    };
    group = lib.mkOption {
      type = lib.types.str;
      default = "rworker";
      description = "Existing group whose members can submit control requests.";
    };
  };

  config = lib.mkMerge [
    (lib.mkIf cfg.enable {
      environment.systemPackages = [ client ];
      systemd = {
        sockets.rworker-control = {
          wantedBy = [ "sockets.target" ];
          socketConfig = {
            Accept = true;
            ListenStream = socket;
            SocketMode = "0660";
            SocketUser = cfg.user;
            SocketGroup = cfg.group;
          };
        };
        services = {
          rworker-inventory = {
            description = "Refresh shared rworker inventory";
            wantedBy = [ "multi-user.target" ];
            wants = [ "network-online.target" ];
            after = [ "network-online.target" ];
            path = runtimeTools;
            environment.RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
            serviceConfig = {
              User = cfg.user;
              Group = cfg.group;
              RuntimeDirectory = "rworker-inventory";
              RuntimeDirectoryMode = "0750";
              UMask = "0027";
              Restart = "on-failure";
              RestartSec = "2s";
              ExecStart = command [
                "server"
                cfg.settingsFile
                "refresh-inventory"
              ];
            };
          };
          rworker-inventory-api = {
            description = "rworker inventory HTTP API";
            wantedBy = [ "multi-user.target" ];
            requires = [ "rworker-inventory.service" ];
            after = [ "rworker-inventory.service" ];
            environment.RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
            serviceConfig = {
              User = cfg.user;
              Group = apiGroup;
              RuntimeDirectory = "rworker-inventory-api";
              RuntimeDirectoryMode = "0750";
              UMask = "0007";
              Restart = "on-failure";
              ExecStart = command [
                "server"
                cfg.settingsFile
                "inventory-api"
                apiSocket
              ];
            };
          };
          "rworker-control@" = {
            description = "rworker control request";
            wants = [ "rworker-inventory.service" ];
            after = [ "rworker-inventory.service" ];
            environment.RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
            path = runtimeTools;
            serviceConfig = {
              User = cfg.user;
              Group = cfg.group;
              ExecStart = command [
                "__serve-control-socket"
                cfg.settingsFile
              ];
              StandardInput = "socket";
              StandardOutput = "socket";
              StandardError = "journal";
            };
          };
        };
      };
    })
    (lib.mkIf (cfg.enable && cfg.api.https != null) {
      users.groups.rworker-inventory-api = { };
      users.users.${config.services.nginx.user}.extraGroups = [ apiGroup ];
      services.nginx = {
        enable = true;
        virtualHosts.${cfg.api.https.serverName} = {
          onlySSL = true;
          listen = [
            {
              addr = cfg.api.https.listenAddress;
              port = 443;
              ssl = true;
            }
          ];
          sslCertificate = cfg.api.https.certificateFile;
          sslCertificateKey = cfg.api.https.certificateKeyFile;
          extraConfig = ''
            ssl_client_certificate ${cfg.api.https.clientCAFile};
            ssl_verify_client on;
          '';
          locations."/v1/workers" = {
            proxyPass = "http://unix:${apiSocket}:";
          };
        };
      };
    })
    (lib.mkIf cfg.reaper.enable {
      systemd.services.rworker-reaper = {
        description = "Reap expired rworker instances";
        path = runtimeTools;
        serviceConfig = {
          Type = "oneshot";
          User = cfg.user;
          Group = cfg.group;
          ExecStart = command [
            "server"
            cfg.settingsFile
            "reap"
          ];
        };
      };
      systemd.timers.rworker-reaper = {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "2m";
          OnUnitActiveSec = "1m";
        };
      };
    })
    (lib.mkIf cfg.autoscaler.enable {
      systemd.services.nixbot-autoscaler = {
        description = "Autoscale the nixbot worker pool";
        wantedBy = [ "multi-user.target" ];
        after = [ "network-online.target" ];
        wants = [ "network-online.target" ];
        path = runtimeTools ++ [ pkgs.postgresql ];
        serviceConfig = {
          User = cfg.autoscaler.user;
          Group = cfg.autoscaler.group;
          ExecStart = command [
            "server"
            cfg.autoscaler.settingsFile
            "autoscale"
          ];
          Restart = "on-failure";
        };
      };
    })
  ];
}
