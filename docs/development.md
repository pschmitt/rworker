# Development

```sh
nix develop
cargo test                      # unit tests, including the spec check
cargo clippy --all-targets -- -D warnings
cargo fmt --check
cargo deny check
nix flake check                 # package, inventory API, MCP test (on a build host)
```

## The command spec

`src/text/spec.txt` lists every action, flag and value set; the shell
completions read it through `rworker __spec`. Value sets named on its
`dynamic` line (topics, agents, agent ids) are completed from the cached
worker list through `rworker __values SET`; the `-re` sets are regex
filters and get escaped values.

The completion scripts (`src/completions/`) are compiled into the binary
and printed by `rworker completions zsh|bash`; the Nix package installs
what the built binary prints. A unit test checks every flag they name
against the spec. A unit test checks that the
spec, the dispatch and the per-action help (`src/text/help-*.txt`) list the
same actions and flags, so a new flag needs all three.

## Inventory API smoke test

The flake checks shared inventory refreshes, ETags and snapshot age against
fake providers. Run the same test against a development build with:

```sh
cargo build
python3 tests/server-parity/inventory-cache.py target/debug/rworker
```
