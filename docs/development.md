# Development

```sh
nix develop
cargo test                      # unit tests, including the spec check
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check
nix flake check                 # package, parity harness, MCP test (on a build host)
```

## The command spec

`src/text/spec.txt` lists every action, flag and value set; the shell
completions read it through `rworker __spec`. A unit test checks that the
spec, the dispatch and the per-action help (`src/text/help-*.txt`) list the
same actions and flags, so a new flag needs all three.

## Parity harness

The Rust client replaces a bash client; [PARITY.md](../PARITY.md) says what
must stay the same. `tests/parity/run.py` runs the bash reference and the
Rust binary over the same corpus of invocations, against a fake control host
and fake workers (`tests/parity/fake/fake.py`; the job manager really runs,
locally), and compares stdout, stderr, exit status, the remote commands and
the state directory:

```sh
cargo build --release
FAKE_SSH_STDIN=0 python3 tests/parity/run.py target/release/rworker   # every case must match
python3 tests/parity/run.py target/release/rworker                    # realistic ssh: known bash bugs accepted
python3 tests/parity/run.py target/release/rworker ls-                # only the cases whose name contains ls-
```

It needs bash, jq, column (util-linux), procps and tput on PATH for the
reference.
