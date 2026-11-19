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
    };
}
