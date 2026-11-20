{
  lib,
  stdenv,
  runCommand,
  # crane's library for this nixpkgs (`crane.mkLib pkgs`).
  craneLib,
  makeWrapper,
  installShellFiles,
  openssh,
  rsync,
  python3Packages,
  writers,
  # Further tools on the client's PATH, ahead of the rest (a wrapped ssh, ...).
  extraRuntimeInputs ? [ ],
  # OpenSSH connection sharing needs a Unix socket; Android's translated
  # app-private paths are too long for one, so TermuxPP turns it off.
  sshControlMaster ? true,
}:
let
  cargoToml = lib.importTOML ../Cargo.toml;
  inherit (cargoToml.package) version;
  # The package's own version, normalized out of everything the dependency
  # build sees, so a version bump keeps that derivation (and its caches).
  normalizedContents =
    file:
    builtins.replaceStrings
      [ "name = \"rworker\"\nversion = \"${version}\"" ]
      [ "name = \"rworker\"\nversion = \"0.0.0\"" ]
      (builtins.readFile file);
  normalized = file: builtins.toFile (baseNameOf file) (normalizedContents file);
  common = {
    pname = "rworker";
    inherit version src;
    strictDeps = true;
    cargoVendorDir = craneLib.vendorCargoDeps {
      cargoLockContents = normalizedContents ../Cargo.lock;
    };
    cargoExtraArgs = "--locked";
  };
  depsSrc = runCommand "rworker-deps-src" { } ''
    mkdir -p $out/src
    cp ${normalized ../Cargo.toml} $out/Cargo.toml
    cp ${normalized ../Cargo.lock} $out/Cargo.lock
    echo 'fn main() {}' > $out/src/main.rs
  '';
  # The dependencies, built once: source changes and version bumps reuse them.
  deps = craneLib.buildDepsOnly (
    builtins.removeAttrs common [ "src" ]
    // {
      pname = "rworker-deps";
      version = "0";
      dummySrc = depsSrc;
    }
  );
  # Only what the binary is built from, so editing docs, Nix files or CI does
  # not rebuild it.
  src = lib.fileset.toSource {
    root = ./..;
    fileset = lib.fileset.unions [
      ../Cargo.toml
      ../Cargo.lock
      ../src
    ];
  };
  # Extras first: a wrapped ssh has to win over openssh's. nix, tput and
  # the host's own tools come from the user's PATH.
  runtimePath = lib.makeBinPath (
    extraRuntimeInputs
    ++ [
      openssh
      rsync
    ]
  );

  # Ad-hoc build workers as MCP tools (up/ls/exec/start/logs/sync/down), on
  # top of the client so host keys and state are shared.
  mcp = writers.writePython3Bin "rworker-mcp" {
    libraries = [ python3Packages.mcp ];
    flakeIgnore = [ "E501" ];
  } (builtins.readFile ../mcp/rworker-mcp.py);

  # The compiled client. It does not depend on extraRuntimeInputs or
  # sshControlMaster, so overriding those (TermuxPP does) only rebuilds the
  # wrapper below, and every consumer shares this binary and its cache entry.
  unwrapped = craneLib.buildPackage (
    common
    // {
      pname = "rworker-unwrapped";
      cargoArtifacts = deps;
      nativeBuildInputs = [ installShellFiles ];
      # The unit tests run here, reusing the built dependencies.
      doCheck = true;

      # The completion scripts are part of the binary (`rworker completions`),
      # so they always match the commands it has.
      postInstall = lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
        $out/bin/rworker completions zsh > _rworker
        $out/bin/rworker completions bash > rworker.bash
        installShellCompletion --zsh _rworker
        installShellCompletion --bash --name rworker rworker.bash
      '';

      meta = meta // {
        description = "${meta.description} (unwrapped)";
      };
    }
  );

  meta = {
    description = "Client for ad-hoc build workers: disposable, encrypted OpenStack VMs";
    homepage = "https://github.com/pschmitt/rworker";
    license = lib.licenses.gpl3Only;
    maintainers = [ lib.maintainers.pschmitt ];
    mainProgram = "rworker";
    platforms = lib.platforms.linux;
  };
in
runCommand "rworker-${version}"
  {
    pname = "rworker";
    inherit version meta;
    nativeBuildInputs = [ makeWrapper ];
    passthru = {
      inherit mcp unwrapped;
      # CI (ci-workflows nix-build) also pushes the crane dependency build
      # and vendored sources it finds on the package.
      inherit (unwrapped) cargoArtifacts cargoVendorDir;
    };
  }
  ''
    makeWrapper ${unwrapped}/bin/rworker $out/bin/rworker \
      --prefix PATH : ${runtimePath} \
      ${lib.optionalString (!sshControlMaster) "--set RWORKER_SSH_CONTROL_MASTER 0"}
    if [ -d ${unwrapped}/share ]
    then
      ln -s ${unwrapped}/share $out/share
    fi
  ''
