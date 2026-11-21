# Home Manager module: programs.rworker. The settings become defaults in the
# client's wrapper (an explicit environment variable still wins), so they
# reach the client however it is started: a shell, a script, the MCP server.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.programs.rworker;
  inherit (lib) mkOption types;

  env = lib.filterAttrs (_: v: v != null) {
    RWORKER_HOST = cfg.controlHost;
    RWORKER_KEY = if cfg.publicKeyFile == null then null else toString cfg.publicKeyFile;
    RWORKER_USER = cfg.user;
    RWORKER_PROBE_TIMEOUT = if cfg.probeTimeout == null then null else toString cfg.probeTimeout;
    RWORKER_ICONS = if cfg.icons then null else "0";
    RWORKER_AWS_CREDENTIALS_FILE =
      if cfg.nixCache.awsCredentialsFile == null then null else toString cfg.nixCache.awsCredentialsFile;
  };

  base = cfg.package.override {
    inherit (cfg) extraRuntimeInputs sshControlMaster;
  };

  client =
    if env == { } then
      base
    else
      pkgs.symlinkJoin {
        name = "rworker-${base.version or "configured"}";
        paths = [ base ];
        nativeBuildInputs = [ pkgs.makeWrapper ];
        # A copy of the wrapper, not a link: wrapProgram must not touch the
        # store path of the package itself.
        postBuild = ''
          rm $out/bin/rworker
          makeWrapper ${lib.getExe base} $out/bin/rworker \
            ${lib.concatStringsSep " " (
              lib.mapAttrsToList (k: v: "--set-default ${k} ${lib.escapeShellArg v}") env
            )}
        '';
        inherit (base) meta;
        passthru = base.passthru or { };
      };

  mcp = pkgs.symlinkJoin {
    name = "rworker-mcp";
    paths = [ base.mcp ];
    nativeBuildInputs = [ pkgs.makeWrapper ];
    postBuild = ''
      rm $out/bin/rworker-mcp
      makeWrapper ${lib.getExe base.mcp} $out/bin/rworker-mcp \
        --add-flags "--rworker ${lib.getExe client}"
    '';
    meta.mainProgram = "rworker-mcp";
  };
in
{
  options.programs.rworker = {
    enable = lib.mkEnableOption "rworker, the client for ad-hoc build workers";

    package = mkOption {
      type = types.package;
      description = "The rworker package; the options below wrap it.";
    };

    controlHost = mkOption {
      type = types.nullOr types.str;
      default = null;
      example = "control.example.com";
      description = ''
        SSH destination of the control host (`RWORKER_HOST`). Its address
        and host key belong in your SSH configuration.
      '';
    };

    publicKeyFile = mkOption {
      type = types.nullOr (types.either types.path types.str);
      default = null;
      example = "/home/alice/.ssh/id_ed25519.pub";
      description = ''
        Public SSH key handed to new workers (`RWORKER_KEY`; the file must
        end in `.pub`). Its private half, next to it, is what Nix builders
        log in with. Null keeps `~/.ssh/id_ed25519.pub`.
      '';
    };

    user = mkOption {
      type = types.nullOr types.str;
      default = null;
      description = "Login user on every worker (`RWORKER_USER`); null picks it per worker.";
    };

    probeTimeout = mkOption {
      type = types.nullOr types.ints.positive;
      default = null;
      description = "Seconds to wait for each overlay address of a worker (`RWORKER_PROBE_TIMEOUT`, default 4).";
    };

    icons = mkOption {
      type = types.bool;
      default = true;
      description = "Nerd Font glyphs for the agent that created a worker.";
    };

    sshControlMaster = mkOption {
      type = types.bool;
      default = true;
      description = ''
        Share one SSH connection to the control host. Turn it off where the
        state directory is too long a path for an OpenSSH control socket.
      '';
    };

    extraRuntimeInputs = mkOption {
      type = types.listOf types.package;
      default = [ ];
      description = "Tools put on the client's PATH ahead of openssh and rsync (a wrapped ssh, ...).";
    };

    nixCache.awsCredentialsFile = mkOption {
      type = types.nullOr (types.either types.path types.str);
      default = null;
      example = "/run/secrets/nix-cache-aws-credentials";
      description = ''
        AWS credentials file whose profiles `setup nix` (and `up`, by
        default) pushes to workers along with the s3:// substituters
        (`RWORKER_AWS_CREDENTIALS_FILE`). Use a read-only key: anyone with
        a shell on the worker can read it. A runtime path such as a
        sops-nix secret keeps it out of the Nix store. Null reads
        `AWS_SHARED_CREDENTIALS_FILE` or `~/.aws/credentials`.
      '';
    };

    mcp.enable = lib.mkEnableOption "the MCP server `rworker-mcp`, bound to this client";

    aliases = mkOption {
      type = types.listOf types.str;
      default = [ ];
      example = [ "rk" ];
      description = "Shell aliases for `rworker` (the zsh completion also answers for `rk`).";
    };

    finalPackage = mkOption {
      type = types.package;
      readOnly = true;
      description = "The configured client.";
    };

    mcp.finalPackage = mkOption {
      type = types.package;
      readOnly = true;
      description = "The MCP server, calling the configured client.";
    };
  };

  config = lib.mkMerge [
    {
      programs.rworker = {
        finalPackage = client;
        mcp.finalPackage = mcp;
      };
    }
    (lib.mkIf cfg.enable {
      home.packages = [ client ] ++ lib.optional cfg.mcp.enable mcp;
      home.shellAliases = lib.genAttrs cfg.aliases (_: "rworker");
    })
  ];
}
