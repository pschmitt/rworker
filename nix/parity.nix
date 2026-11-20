# The differential parity harness (tests/parity) as a flake check: the bash
# reference client and the Rust binary over the same corpus, once with a
# realistic ssh and once with one that leaves stdin alone.
{
  lib,
  runCommand,
  rworker,
  bashInteractive,
  coreutils,
  curl,
  gawk,
  gnugrep,
  gnused,
  hostname,
  jq,
  ncurses,
  openssh,
  procps,
  python3,
  util-linux,
}:
runCommand "rworker-parity"
  {
    nativeBuildInputs = [
      bashInteractive
      coreutils
      curl
      gawk
      gnugrep
      gnused
      hostname
      jq
      ncurses
      openssh
      procps
      python3
      util-linux
    ];
    src = lib.fileset.toSource {
      root = ./..;
      fileset = lib.fileset.unions [
        ../tests/parity
        ../tests/server-parity/inventory-cache.py
      ];
    };
  }
  ''
    cp -r $src/tests .
    chmod -R u+w tests
    patchShebangs tests
    export PARITY_DIR=$TMPDIR/parity
    unwrapped=${rworker.unwrapped}/bin/rworker
    FAKE_SSH_STDIN=0 python3 tests/parity/run.py $unwrapped
    python3 tests/parity/run.py $unwrapped
    python3 tests/server-parity/inventory-cache.py $unwrapped
    touch $out
  ''
