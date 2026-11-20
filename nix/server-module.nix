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
  builderSocket = "/run/rworker-builder.sock";
  builderAccount = "rworker-builder";
  builderState = "/var/lib/rworker-builder-gateway";
  builderGate = pkgs.writeShellApplication {
    name = "rworker-builder-gate";
    runtimeInputs = [
      pkgs.coreutils
      pkgs.socat
    ];
    text = ''
      case "''${SSH_ORIGINAL_COMMAND:-}" in
        "nix-daemon --stdio" | "nix daemon --stdio")
          {
            printf '%s\n' "''${SSH_CONNECTION%% *}"
            cat
          } | socat -t60 STDIO UNIX-CONNECT:${builderSocket}
          ;;
        *)
          echo "This account serves the Nix daemon protocol." >&2
          exit 126
          ;;
      esac
    '';
  };
  builderCommand =
    mode:
    command [
      "__builder-gateway"
      cfg.settingsFile
      builderState
      mode
      cfg.builderGateway.profile
      (toString cfg.builderGateway.leaseSeconds)
      (toString cfg.builderGateway.idleSeconds)
      (if cfg.builderGateway.clientNamesFile == null then "-" else cfg.builderGateway.clientNamesFile)
    ];
  builderLogLimits = "${toString cfg.logCache.retentionDays}:${toString cfg.logCache.maxMiB}:${toString cfg.logCache.maxJobMiB}";
  builderService = {
    User = cfg.user;
    Group = cfg.group;
    StateDirectory = "rworker-builder-gateway";
    StateDirectoryMode = "0700";
    UMask = "0077";
  };
  client = pkgs.symlinkJoin {
    name = "rworker-local-client";
    paths = [ cfg.package ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = ''
      wrapProgram "$out/bin/rworker" \
        --set RWORKER_CONTROL_SOCKET ${socket} \
        --set RWORKER_CONTROL_SSH_COMPAT 1 \
        ${lib.optionalString cfg.builderGateway.enable "--set RWORKER_BUILDER_ARCHIVE 1"} \
        ${lib.optionalString (cfg.logCache.operator != null) (
          "--set RWORKER_LOG_ARCHIVE_LIMITS "
          + lib.escapeShellArg "${toString cfg.logCache.retentionDays}:${toString cfg.logCache.maxMiB}:${toString cfg.logCache.maxJobMiB}"
        )}
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
    builderGateway = {
      enable = lib.mkEnableOption "the on-demand SSH Nix builder gateway";
      authorizedKeysFile = lib.mkOption {
        type = lib.types.str;
        example = "/run/secrets/rworker-builder-authorized-keys";
        description = "Runtime authorized_keys file readable by the dedicated gateway SSH account.";
      };
      profile = lib.mkOption {
        type = lib.types.str;
        default = "standard";
        description = "Worker profile to provision for remote Nix builds.";
      };
      clientNamesFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "/run/secrets/rworker-builder-client-names.json";
        description = "Runtime JSON mapping SSH peer IP addresses to client machine names. Unmapped clients are identified by their IP.";
      };
      leaseSeconds = lib.mkOption {
        type = lib.types.ints.positive;
        default = 7200;
        description = "Maximum requested worker lifetime, subject to the normal controller limit.";
      };
      idleSeconds = lib.mkOption {
        type = lib.types.ints.positive;
        default = 300;
        description = "Keep the leased worker warm for this long after its last SSH session ends.";
      };
      maxConnections = lib.mkOption {
        type = lib.types.ints.positive;
        default = 8;
        description = "Maximum simultaneous gateway connections sharing the leased worker.";
      };
    };
    builderClient = {
      enable = lib.mkEnableOption "the on-demand rworker entry in /etc/nix/machines";
      hostName = lib.mkOption {
        type = lib.types.str;
        example = "builder.example.com";
        description = "SSH host name of the rworker builder gateway.";
      };
      identityFile = lib.mkOption {
        type = lib.types.str;
        example = "/run/secrets/rworker-builder-key";
        description = "Runtime SSH private key path readable by the local Nix daemon.";
      };
      publicHostKey = lib.mkOption {
        type = lib.types.str;
        description = "Base64-encoded SSH public host-key line of the stable gateway.";
      };
      systems = lib.mkOption {
        type = lib.types.nonEmptyListOf lib.types.str;
        default = [ pkgs.stdenv.hostPlatform.system ];
        description = "Systems the configured worker profile can build.";
      };
      maxJobs = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1;
        description = "Maximum concurrent builds Nix schedules on this entry from this client.";
      };
      speedFactor = lib.mkOption {
        type = lib.types.ints.positive;
        default = 1;
        description = "Relative speed Nix uses when choosing among builders.";
      };
      supportedFeatures = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Nix system features the worker image provides.";
      };
      mandatoryFeatures = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [ ];
        description = "Features a derivation must request before Nix selects this builder.";
      };
    };
    logCache = {
      operator = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Unix account that collects and reads cached job logs. It must already have pinned SSH access to its workers without an interactive agent.";
      };
      identityFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Dedicated worker SSH private key path on the control host.";
      };
      workerUser = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "agent";
        description = "Override the remote worker login for collection when jobs run under a separate account.";
      };
      retentionDays = lib.mkOption {
        type = lib.types.ints.positive;
        default = 7;
        description = "Maximum age from the source log's last modification.";
      };
      maxMiB = lib.mkOption {
        type = lib.types.ints.positive;
        default = 2048;
        description = "Maximum total retained log bytes, in MiB.";
      };
      maxJobMiB = lib.mkOption {
        type = lib.types.ints.between 1 256;
        default = 32;
        description = "Maximum retained tail per job, in MiB.";
      };
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
            wants = [ "rworker-inventory.service" ];
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
            environment = {
              RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
            }
            // lib.optionalAttrs cfg.builderGateway.enable {
              RWORKER_BUILDER_ARCHIVE_DIRECTORY = builderState;
            };
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
    (lib.mkIf cfg.builderGateway.enable {
      assertions = [
        {
          assertion = cfg.enable;
          message = "services.rworker.builderGateway requires services.rworker.enable.";
        }
        {
          assertion =
            lib.hasPrefix "/" cfg.builderGateway.authorizedKeysFile
            && builtins.match "[^[:space:]]+" cfg.builderGateway.authorizedKeysFile != null;
          message = "The rworker builder gateway authorizedKeysFile must be an absolute runtime path without whitespace.";
        }
      ];
      users.groups.${builderAccount} = { };
      users.users.${builderAccount} = {
        isSystemUser = true;
        group = builderAccount;
        shell = pkgs.bash;
      };
      services.openssh = {
        enable = true;
        extraConfig = ''
          Match User ${builderAccount}
            AuthorizedKeysFile ${cfg.builderGateway.authorizedKeysFile}
            ForceCommand ${lib.getExe builderGate}
            AuthenticationMethods publickey
            DisableForwarding yes
            PermitTTY no
            PermitUserRC no
          Match All
        '';
      };
      systemd = {
        sockets.rworker-builder = {
          wantedBy = [ "sockets.target" ];
          socketConfig = {
            Accept = true;
            ListenStream = builderSocket;
            SocketMode = "0660";
            SocketUser = cfg.user;
            SocketGroup = builderAccount;
            MaxConnections = cfg.builderGateway.maxConnections;
          };
        };
        services = {
          "rworker-builder@" = {
            environment.RWORKER_BUILDER_LOG_LIMITS = builderLogLimits;
            description = "On-demand rworker Nix daemon connection";
            environment.RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
            path = runtimeTools;
            serviceConfig = builderService // {
              ExecStart = builderCommand "stdio";
              StandardInput = "socket";
              StandardOutput = "socket";
              StandardError = "journal";
            };
          };
          rworker-builder-reap = {
            environment.RWORKER_BUILDER_LOG_LIMITS = builderLogLimits;
            description = "Delete the idle rworker builder lease";
            environment.RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
            path = runtimeTools;
            serviceConfig = builderService // {
              Type = "oneshot";
              ExecStart = builderCommand "reap";
            };
          };
        };
        timers.rworker-builder-reap = {
          wantedBy = [ "timers.target" ];
          timerConfig = {
            OnBootSec = "1m";
            OnUnitInactiveSec = "1m";
            AccuracySec = "5s";
          };
        };
      };
    })
    (lib.mkIf cfg.builderClient.enable {
      assertions = [
        {
          assertion =
            lib.hasPrefix "/" cfg.builderClient.identityFile
            && builtins.match "[^[:space:]]+" cfg.builderClient.identityFile != null
            && builtins.match "[A-Za-z0-9+/=]+" cfg.builderClient.publicHostKey != null;
          message = "The rworker builder client needs an absolute runtime identity path and a pinned base64 SSH host key.";
        }
      ];
      nix = {
        distributedBuilds = true;
        buildMachines = [
          {
            protocol = "ssh-ng";
            sshUser = builderAccount;
            sshKey = cfg.builderClient.identityFile;
            inherit (cfg.builderClient)
              hostName
              publicHostKey
              systems
              maxJobs
              speedFactor
              supportedFeatures
              mandatoryFeatures
              ;
          }
        ];
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
    (lib.mkIf (cfg.enable && cfg.logCache.operator != null) {
      assertions = [
        {
          assertion = cfg.logCache.maxJobMiB <= cfg.logCache.maxMiB;
          message = "services.rworker.logCache.maxJobMiB must not exceed maxMiB";
        }
      ];
      systemd.services.rworker-log-cache = {
        description = "Collect the operator's registered rworker job logs";
        path = [
          pkgs.coreutils
          pkgs.openssh
        ];
        environment = {
          RWORKER_CONTROL_SOCKET = socket;
        }
        // lib.optionalAttrs (cfg.logCache.workerUser != null) {
          RWORKER_USER = cfg.logCache.workerUser;
        }
        // lib.optionalAttrs (cfg.logCache.identityFile != null) {
          RWORKER_LOG_CACHE_IDENTITY = cfg.logCache.identityFile;
        };
        serviceConfig = {
          Type = "oneshot";
          User = cfg.logCache.operator;
          UMask = "0077";
          TimeoutStartSec = "5min";
          ExecStart = command [
            "__collect-logs"
            (toString cfg.logCache.retentionDays)
            (toString cfg.logCache.maxMiB)
            (toString cfg.logCache.maxJobMiB)
          ];
        };
      };
      systemd.timers.rworker-log-cache = {
        wantedBy = [ "timers.target" ];
        timerConfig = {
          OnBootSec = "30s";
          OnUnitInactiveSec = "30s";
          AccuracySec = "5s";
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
