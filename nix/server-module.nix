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
  eventDirectory = "/var/lib/rworker-events";
  eventEnvironment = {
    RWORKER_EVENT_DIRECTORY = eventDirectory;
    RWORKER_EVENT_RETENTION_DAYS = toString cfg.logCache.retentionDays;
  };
  logReaderEnvironment = {
    RWORKER_CONTROL_SOCKET = socket;
  }
  // lib.optionalAttrs (cfg.logCache.workerUser != null) {
    RWORKER_USER = cfg.logCache.workerUser;
  }
  // lib.optionalAttrs (cfg.logCache.identityFile != null) {
    RWORKER_LOG_CACHE_IDENTITY = cfg.logCache.identityFile;
  };
  socket = "/run/rworker-control.sock";
  webOrigin =
    if cfg.web.origin != null then
      cfg.web.origin
    else if cfg.api.https != null then
      "https://${cfg.api.https.serverName}"
    else
      null;
  webWorkerAccess =
    cfg.web.enable
    && cfg.web.operator != null
    && cfg.web.identityFile != null
    && cfg.web.publicKeyFile != null;
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
    web = {
      enable = lib.mkEnableOption "the HTTPS rworker operator console";
      operator = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = cfg.logCache.operator;
        description = "Local account whose rworker permissions and SSH identity the web console uses.";
      };
      origin = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        example = "https://console.example.com";
        description = "HTTPS origin of an independently configured authenticated reverse proxy. Defaults to the built-in HTTPS API hostname.";
      };
      proxyUser = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Single authenticated Remote-User accepted from a trusted reverse proxy that overwrites the header on every console route.";
      };
      signOutUrl = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "HTTPS sign-out URL of a trusted reverse proxy's identity provider.";
      };
      identityFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Private SSH identity used by the console for worker connections.";
      };
      tokenFile = lib.mkOption {
        type = lib.types.str;
        example = "/run/secrets/rworker-web-token";
        description = "Runtime file containing a random token of at least 32 characters, readable by the console operator.";
      };
      publicKeyFile = lib.mkOption {
        type = lib.types.nullOr lib.types.str;
        default = null;
        description = "Console SSH public key. With identityFile set, provisioning also authorizes it on the console operator’s workers.";
      };
    };
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
    githubActions = {
      enable = lib.mkEnableOption "GitHub Actions workers and their SSH relay";
      serverName = lib.mkOption {
        type = lib.types.str;
        example = "rworker-agent.example.com";
        description = "Public HTTPS hostname for GitHub runner registration.";
      };
      authorizedKeysFile = lib.mkOption {
        type = lib.types.str;
        example = "/run/secrets/rworker-github-relay-keys";
        description = "Runtime authorized_keys file readable by the GitHub relay account.";
      };
    };
    gitlabCi = {
      enable = lib.mkEnableOption "GitLab CI workers and their SSH relay";
      serverName = lib.mkOption {
        type = lib.types.str;
        example = "rworker-agent.example.com";
        description = "Public HTTPS hostname for GitLab runner registration.";
      };
      authorizedKeysFile = lib.mkOption {
        type = lib.types.str;
        example = "/run/secrets/rworker-gitlab-relay-keys";
        description = "Runtime authorized_keys file readable by the GitLab relay account.";
      };
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
      unitName = lib.mkOption {
        type = lib.types.str;
        default = "nixbot-autoscaler";
        example = "circus-autoscaler";
        description = ''
          Systemd unit of the autoscaler, so a host can run a pool other than
          nixbot's (for example one scaled on Circus demand) under its own name.
        '';
      };
      description = lib.mkOption {
        type = lib.types.str;
        default = "Autoscale the nixbot worker pool";
        description = "Description of the autoscaler unit.";
      };
    };
    autoscalers = lib.mkOption {
      type = lib.types.attrsOf (
        lib.types.submodule (
          { name, ... }:
          {
            options = {
              settingsFile = lib.mkOption {
                type = lib.types.str;
                description = "Runtime JSON settings path for this autoscaler.";
              };
              user = lib.mkOption {
                type = lib.types.str;
                default = "rworker";
                description = "Existing account running this autoscaler.";
              };
              group = lib.mkOption {
                type = lib.types.str;
                default = "rworker";
                description = "Existing group running this autoscaler.";
              };
              description = lib.mkOption {
                type = lib.types.str;
                default = "Autoscale the ${name} worker pool";
                description = "Systemd unit description.";
              };
            };
          }
        )
      );
      default = { };
      description = ''
        Additional named autoscaler services. Attribute names are systemd
        unit names; the legacy `autoscaler` option remains available for a
        single existing pool.
      '';
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
    {
      assertions = [
        {
          assertion = !cfg.autoscaler.enable || !(builtins.hasAttr cfg.autoscaler.unitName cfg.autoscalers);
          message = "The legacy rworker autoscaler unitName must not collide with a named autoscaler.";
        }
      ];
    }
    (lib.mkIf cfg.web.enable {
      assertions = [
        {
          assertion =
            cfg.enable && webOrigin != null && lib.hasPrefix "https://" webOrigin && cfg.web.operator != null;
          message = "The rworker web console requires the server, an HTTPS API and an operator account.";
        }
        {
          assertion = lib.hasPrefix "/" cfg.web.tokenFile;
          message = "The rworker web tokenFile must be an absolute runtime path.";
        }
        {
          assertion = cfg.web.proxyUser == null || cfg.web.origin != null;
          message = "Trusted proxy authentication requires an explicitly configured web.origin.";
        }
      ];
    })
    (lib.mkIf cfg.enable {
      environment.systemPackages = [ client ];
      systemd.tmpfiles.rules = [
        "d ${eventDirectory} 2770 ${cfg.user} ${cfg.group} -"
      ];
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
            environment = eventEnvironment // {
              RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
            };
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
            wants = [ "rworker-inventory.service" ] ++ lib.optional cfg.web.enable "rworker-control.socket";
            after = [ "rworker-inventory.service" ] ++ lib.optional cfg.web.enable "rworker-control.socket";
            path = lib.optionals cfg.web.enable [
              pkgs.coreutils
              pkgs.openssh
            ];
            environment = {
              RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
            }
            // lib.optionalAttrs (cfg.web.enable && webOrigin != null) {
              RWORKER_WEB_ORIGIN = webOrigin;
              RWORKER_WEB_TOKEN_FILE = cfg.web.tokenFile;
              RWORKER_EVENT_DIRECTORY = eventDirectory;
              RWORKER_CONTROL_SOCKET = socket;
            }
            // lib.optionalAttrs (cfg.web.enable && cfg.web.proxyUser != null) {
              RWORKER_WEB_PROXY_USER = cfg.web.proxyUser;
            }
            // lib.optionalAttrs (cfg.web.enable && cfg.web.signOutUrl != null) {
              RWORKER_WEB_SIGN_OUT_URL = cfg.web.signOutUrl;
            }
            //
              lib.optionalAttrs
                (cfg.web.enable && cfg.web.operator == cfg.logCache.operator && cfg.logCache.workerUser != null)
                {
                  RWORKER_USER = cfg.logCache.workerUser;
                }
            // lib.optionalAttrs (cfg.web.enable && cfg.web.identityFile != null) {
              RWORKER_IDENTITY_FILE = cfg.web.identityFile;
            }
            // lib.optionalAttrs (cfg.web.enable && cfg.web.publicKeyFile != null) {
              RWORKER_KEY = cfg.web.publicKeyFile;
            }
            // lib.optionalAttrs (cfg.web.enable && cfg.builderGateway.enable) {
              RWORKER_BUILDER_ARCHIVE = "1";
            };
            serviceConfig = {
              User = if cfg.web.enable && cfg.web.operator != null then cfg.web.operator else cfg.user;
              SupplementaryGroups = lib.optional cfg.web.enable cfg.group;
              Group = apiGroup;
              RuntimeDirectory = "rworker-inventory-api";
              RuntimeDirectoryMode = "0750";
              UMask = if cfg.web.enable then "0077" else "0007";
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
            environment =
              eventEnvironment
              // {
                RWORKER_INVENTORY_DIRECTORY = inventoryDirectory;
              }
              // lib.optionalAttrs webWorkerAccess {
                RWORKER_WEB_WORKER_OWNER = cfg.web.operator;
              }
              // lib.optionalAttrs cfg.builderGateway.enable {
                RWORKER_BUILDER_ARCHIVE_DIRECTORY = builderState;
              };
            path = runtimeTools;
            serviceConfig = {
              User = cfg.user;
              Group = cfg.group;
              LoadCredential = lib.optional webWorkerAccess "rworker-web-worker-public-key:${cfg.web.publicKeyFile}";
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
    (lib.mkMerge (
      lib.mapAttrsToList
        (
          provider: options:
          let
            account = "rworker-${provider}-relay";
            apiGroup = "rworker-${provider}-api";
            apiSocket = "/run/${apiGroup}/http.sock";
          in
          lib.mkIf options.enable {
            assertions = [
              {
                assertion = cfg.enable;
                message = "The rworker ${provider} integration requires services.rworker.enable.";
              }
              {
                assertion =
                  lib.hasPrefix "/" options.authorizedKeysFile
                  && builtins.match "[^[:space:]]+" options.authorizedKeysFile != null;
                message = "The rworker ${provider} relay authorizedKeysFile must be an absolute runtime path without whitespace.";
              }
            ];
            users = {
              groups = {
                ${account} = { };
                ${apiGroup} = { };
              };
              users = {
                ${account} = {
                  isSystemUser = true;
                  group = account;
                  shell = pkgs.bash;
                };
                ${config.services.nginx.user}.extraGroups = [ apiGroup ];
              };
            };
            services.openssh = {
              enable = true;
              extraConfig = ''
                Match User ${account}
                  AuthorizedKeysFile ${options.authorizedKeysFile}
                  AuthenticationMethods publickey
                  MaxSessions 0
                  AllowTcpForwarding remote
                  AllowStreamLocalForwarding no
                  GatewayPorts no
                  PermitListen 127.0.0.1:*
                  PermitTunnel no
                  PermitTTY no
                  PermitUserRC no
                Match All
              '';
            };
            services.nginx = {
              enable = true;
              virtualHosts.${options.serverName} = {
                forceSSL = true;
                enableACME = true;
                locations = {
                  "/".return = "404";
                  "/v1/agent/jobs/" = {
                    proxyPass = "http://unix:${apiSocket}:";
                    extraConfig = "client_max_body_size 4k;";
                  };
                };
              };
            };
            systemd.services = {
              ${apiGroup} = {
                description = "rworker ${provider} runner API";
                wantedBy = [ "multi-user.target" ];
                serviceConfig = {
                  User = cfg.user;
                  Group = apiGroup;
                  SupplementaryGroups = [ cfg.group ];
                  RuntimeDirectory = apiGroup;
                  RuntimeDirectoryMode = "0750";
                  StateDirectory = "rworker-${provider}";
                  StateDirectoryMode = "0700";
                  UMask = "0077";
                  Restart = "on-failure";
                  ExecStartPre = "${pkgs.coreutils}/bin/rm -f ${apiSocket}";
                  ExecStart = command [
                    "server"
                    cfg.settingsFile
                    "github-agent-api"
                    apiSocket
                  ];
                };
              };
              "rworker-${provider}-reap" = {
                description = "Reconcile ${provider} worker leases";
                after = [ "${apiGroup}.service" ];
                serviceConfig = {
                  Type = "oneshot";
                  User = cfg.user;
                  Group = cfg.group;
                  ExecStart = command [
                    "server"
                    cfg.settingsFile
                    "${provider}-reap"
                  ];
                };
              };
            };
            systemd.timers."rworker-${provider}-reap" = {
              wantedBy = [ "timers.target" ];
              timerConfig = {
                OnBootSec = "1m";
                OnUnitInactiveSec = "1m";
              };
            };
          }
        )
        {
          github = cfg.githubActions;
          gitlab = cfg.gitlabCi;
        }
    ))
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
          locations = {
            "/v1/workers".proxyPass = "http://unix:${apiSocket}:";
            "= /v1/workers/events/ws" = {
              proxyPass = "http://unix:${apiSocket}:";
              proxyWebsockets = true;
            };
          }
          // lib.optionalAttrs (cfg.web.enable && cfg.web.origin == null) {
            "/".proxyPass = "http://unix:${apiSocket}:";
            "= /v1/ui/updates" = {
              proxyPass = "http://unix:${apiSocket}:";
              proxyWebsockets = true;
            };
            "/v1/ui/follow/" = {
              proxyPass = "http://unix:${apiSocket}:";
              proxyWebsockets = true;
            };
            "/v1/ui/action" = {
              proxyPass = "http://unix:${apiSocket}:";
              extraConfig = "proxy_read_timeout 920s;";
            };
            "= /v1/automation/workers" = {
              proxyPass = "http://unix:${apiSocket}:";
              proxyWebsockets = true;
            };
            "= /v1/automation/action" = {
              proxyPass = "http://unix:${apiSocket}:";
              extraConfig = "proxy_read_timeout 3620s;";
            };
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
      systemd = {
        services.rworker-log-cache = {
          description = "Collect the operator's registered rworker job logs";
          path = [
            pkgs.coreutils
            pkgs.openssh
          ];
          environment = logReaderEnvironment;
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
        services.rworker-job-events = {
          description = "Observe registered rworker job lifecycle events";
          wantedBy = [ "multi-user.target" ];
          wants = [
            "rworker-control.socket"
            "rworker-inventory.service"
          ];
          after = [
            "rworker-control.socket"
            "rworker-inventory.service"
          ];
          path = [ pkgs.openssh ];
          environment =
            eventEnvironment
            // logReaderEnvironment
            // {
              XDG_STATE_HOME = "/var/lib/rworker-job-events";
            };
          serviceConfig = {
            User = cfg.logCache.operator;
            SupplementaryGroups = [ cfg.group ];
            StateDirectory = "rworker-job-events";
            StateDirectoryMode = "0700";
            UMask = "0077";
            Restart = "on-failure";
            ExecStart = command [ "__collect-job-events" ];
          };
        };
        timers.rworker-log-cache = {
          wantedBy = [ "timers.target" ];
          timerConfig = {
            OnBootSec = "30s";
            OnUnitInactiveSec = "30s";
            AccuracySec = "5s";
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
      systemd.services.${cfg.autoscaler.unitName} = {
        inherit (cfg.autoscaler) description;
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
    (lib.mkIf (cfg.autoscalers != { }) {
      systemd.services = lib.mapAttrs' (
        unitName: autoscaler:
        lib.nameValuePair unitName {
          inherit (autoscaler) description;
          wantedBy = [ "multi-user.target" ];
          after = [ "network-online.target" ];
          wants = [ "network-online.target" ];
          path = runtimeTools ++ [ pkgs.postgresql ];
          serviceConfig = {
            User = autoscaler.user;
            Group = autoscaler.group;
            ExecStart = command [
              "server"
              autoscaler.settingsFile
              "autoscale"
            ];
            Restart = "on-failure";
          };
        }
      ) cfg.autoscalers;
    })
  ];
}
