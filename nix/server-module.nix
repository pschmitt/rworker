{
  config,
  lib,
  pkgs,
  utils,
  ...
}:
let
  cfg = config.services.rworker;
  socket = "/run/rworker-control.sock";
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
    reaper.enable = lib.mkEnableOption "periodic reaping of expired ad-hoc workers";
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
      systemd.sockets.rworker-control = {
        wantedBy = [ "sockets.target" ];
        socketConfig = {
          Accept = true;
          ListenStream = socket;
          SocketMode = "0660";
          SocketUser = cfg.user;
          SocketGroup = cfg.group;
        };
      };
      systemd.services."rworker-control@" = {
        description = "rworker control request";
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
