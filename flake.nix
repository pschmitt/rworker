{
  description = "Client for ad-hoc build workers: disposable, encrypted OpenStack VMs";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    # Builds the dependencies in their own derivation, so CI caches and hosts
    # reuse them instead of recompiling every crate on each change.
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      crane,
    }:
    let
      packageFor = pkgs: pkgs.callPackage ./nix/package.nix { craneLib = crane.mkLib pkgs; };
      eachSystem = flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (
        system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          rworker = packageFor pkgs;
        in
        {
          packages = {
            default = rworker;
            inherit rworker;
            rworker-mcp = rworker.mcp;
          };

          apps.default = {
            type = "app";
            program = "${rworker}/bin/rworker";
          };

          # The package build runs the unit tests.
          checks = {
            inherit rworker;
            parity = pkgs.callPackage ./nix/parity.nix { inherit rworker; };
            # The Home Manager module, evaluated against stand-ins for the two
            # Home Manager options it sets, and its configured wrapper built
            # and asked for its settings.
            hm-module =
              let
                eval = nixpkgs.lib.evalModules {
                  modules = [
                    self.homeModules.default
                    {
                      options.home = {
                        packages = nixpkgs.lib.mkOption { type = nixpkgs.lib.types.listOf nixpkgs.lib.types.package; };
                        shellAliases = nixpkgs.lib.mkOption { type = nixpkgs.lib.types.attrsOf nixpkgs.lib.types.str; };
                      };
                      config = {
                        _module.args.pkgs = pkgs;
                        programs.rworker = {
                          enable = true;
                          controlHost = "control.example.com";
                          publicKeyFile = "/keys/id_test.pub";
                          icons = false;
                          sshControlMaster = false;
                          nixCache.awsCredentialsFile = "/run/secrets/aws";
                          mcp.enable = true;
                          aliases = [ "rk" ];
                        };
                      };
                    }
                  ];
                };
                cfg = eval.config;
                client = cfg.programs.rworker.finalPackage;
              in
              assert cfg.home.shellAliases.rk == "rworker";
              assert builtins.length cfg.home.packages == 2;
              pkgs.runCommand "rworker-hm-module-check" { } ''
                wrapper=${client}/bin/rworker
                grep -q 'RWORKER_HOST.*control.example.com' $wrapper
                grep -q 'RWORKER_KEY.*/keys/id_test.pub' $wrapper
                grep -q 'RWORKER_ICONS.*0' $wrapper
                grep -q 'RWORKER_AWS_CREDENTIALS_FILE.*/run/secrets/aws' $wrapper
                # sshControlMaster is the package's own wrapper, which this one runs.
                grep -q 'RWORKER_SSH_CONTROL_MASTER.*0' "$(grep -o '/nix/store/[^"]*/bin/rworker' $wrapper)"
                grep -q -- '--rworker ${client}/bin/rworker' ${cfg.programs.rworker.mcp.finalPackage}/bin/rworker-mcp
                $wrapper __spec | grep -q '^action up '
                test -e ${client}/share/zsh/site-functions/_rworker
                touch $out
              '';
            mcp =
              pkgs.runCommand "rworker-mcp-test"
                {
                  nativeBuildInputs = [ pkgs.python3 ];
                }
                ''
                  cp -r ${./mcp} mcp
                  cp -r ${./tests} tests
                  python3 tests/mcp/test-rworker-mcp.py
                  touch $out
                '';
          };

          formatter = pkgs.nixfmt;

          devShells.default = pkgs.mkShell {
            inputsFrom = [ rworker ];
            packages = with pkgs; [
              cargo
              rustc
              clippy
              rustfmt
              cargo-deny
              rust-analyzer
              nixfmt
              statix
              deadnix
              # The parity harness's reference needs these.
              jq
              util-linux
              procps
              ncurses
              python3
            ];
          };
        }
      );
    in
    eachSystem
    // {
      # x86_64-linux package builds and checks run on the self-hosted nixbot;
      # aarch64-linux builds natively on GitHub's arm runners (the nix-aarch64
      # jobs of .github/workflows/build.yaml).
      hydraJobs = nixpkgs.lib.genAttrs [ "x86_64-linux" ] (system: {
        packages = self.packages.${system};
        checks = self.checks.${system};
      });

      overlays.default = _final: prev: {
        rworker = packageFor prev;
      };

      # programs.rworker; this flake's package is the default.
      homeModules.default =
        { lib, pkgs, ... }:
        {
          imports = [ ./nix/hm-module.nix ];
          programs.rworker.package = lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        };
      homeModules.rworker = self.homeModules.default;
      homeManagerModules = self.homeModules;
    };
}
