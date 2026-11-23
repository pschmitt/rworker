{
  description = "Custom rworker OpenStack image";

  inputs = {
    rworker.url = "github:pschmitt/rworker";
    nixpkgs.follows = "rworker/nixpkgs";
  };

  outputs =
    { nixpkgs, rworker, ... }:
    {
      nixosConfigurations.rworker = nixpkgs.lib.nixosSystem {
        system = "x86_64-linux";
        modules = [
          rworker.nixosModules.workerImageOpenStack
          (
            { pkgs, ... }:
            {
              system.stateVersion = "26.11";
              environment.systemPackages = [ pkgs.ripgrep ];
            }
          )
        ];
      };
    };
}
