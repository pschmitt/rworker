# The login account of every rworker worker and its job directory, the same on
# all NixOS worker images (this flake's OpenStack image and the images built
# on `nixosModules.workerImage` elsewhere). CI leases set up the same account
# and directory at runtime (scripts/github-job-agent.py, nix/lease-image.nix).
{
  config,
  lib,
  pkgs,
  ...
}:
let
  cfg = config.rworker.worker;
in
{
  options.rworker.worker = {
    user = lib.mkOption {
      type = lib.types.str;
      default = "rworker";
      readOnly = true;
      description = ''
        The account clients log into, with a group of the same name. Images
        publish it as the Glance property `rworker_login_user`, from which the
        controller learns the account of each worker.
      '';
    };
    jobsDirectory = lib.mkOption {
      type = lib.types.str;
      default = "/var/lib/rworker/jobs";
      readOnly = true;
      description = ''
        Where the rworker job tools keep the worker's jobs (exported as
        `RWORKER_JOBS_DIR`): owned by the account, setgid and writable for its
        group.
      '';
    };
    jobMembers = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [ ];
      example = [ "alice" ];
      description = "Other accounts on the worker that read and write its jobs (members of the account's group).";
    };
  };

  config = {
    users.groups.${cfg.user} = { };
    users.users = {
      ${cfg.user} = {
        isNormalUser = true;
        group = cfg.user;
        shell = pkgs.bash;
      };
    }
    // lib.genAttrs cfg.jobMembers (_: {
      extraGroups = [ cfg.user ];
    });

    systemd.tmpfiles.rules = [
      "d ${dirOf cfg.jobsDirectory} 0755 ${cfg.user} ${cfg.user} -"
      "d ${cfg.jobsDirectory} 2770 ${cfg.user} ${cfg.user} -"
    ];

    # Through PAM, so SSH command sessions (`bash -s` of the job tools) have it
    # too, not only login shells.
    environment.sessionVariables.RWORKER_JOBS_DIR = cfg.jobsDirectory;

    nix.settings.trusted-users = [ cfg.user ];
  };
}
