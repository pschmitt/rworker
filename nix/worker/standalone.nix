{ config, pkgs, ... }:
{
  boot.kernelParams = [
    "console=tty0"
    "console=ttyS0,115200"
  ];

  networking.useNetworkd = true;

  services = {
    cloud-init = {
      enable = true;
      settings = {
        datasource_list = [ "OpenStack" ];
        users = [ "default" ];
        disable_root = true;
        # The stock NixOS module uses root and omits cloud-init's SSH module.
        cloud_init_modules = [
          "seed_random"
          "update_hostname"
          "users-groups"
          "ssh"
        ];
        ssh_deletekeys = false;
        ssh_genkeytypes = [ ];
        system_info = {
          distro = "nixos";
          network.renderers = [ "networkd" ];
          default_user = {
            name = config.rworker.worker.user;
            lock_passwd = true;
          };
        };
      };
    };
    openssh = {
      enable = true;
      openFirewall = true;
      settings = {
        PasswordAuthentication = false;
        KbdInteractiveAuthentication = false;
        PermitRootLogin = "no";
        AllowUsers = [ config.rworker.worker.user ];
      };
    };
  };

  nix.settings = {
    experimental-features = [
      "nix-command"
      "flakes"
    ];
  };

  environment.systemPackages = [
    pkgs.git
    pkgs.jq
    pkgs.rsync
    pkgs.tmux
  ];
  documentation.enable = false;
}
