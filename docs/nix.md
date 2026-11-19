# Nix

## Flake outputs

```sh
nix build github:pschmitt/rworker               # the client with its completions
nix run github:pschmitt/rworker -- help
nix develop                                     # cargo, clippy, rustfmt, cargo-deny, the harness's tools
nix flake check                                 # unit tests, parity harness, MCP test
```

- `packages.<system>.rworker` (also `default`), for x86_64-linux and
  aarch64-linux; `packages.<system>.rworker-mcp` is the MCP server (also
  `rworker.mcp`).
- `overlays.default` adds `pkgs.rworker`.

The package is built with [crane](https://github.com/ipetkov/crane): its
dependencies are a derivation of their own, so source changes and version
bumps reuse them from the cache.

## Package options

```nix
pkgs.rworker.override {
  extraRuntimeInputs = [ pkgs.hostname ];   # tools ahead of openssh on its PATH
  sshControlMaster = false;                 # no shared control-host connection
}
```

`sshControlMaster = false` is for systems whose state paths are too long for
an OpenSSH control socket (Android app storage); it sets
`RWORKER_SSH_CONTROL_MASTER=0` in the wrapper.

## MCP server

`rworker-mcp --rworker PATH` serves the actions as MCP tools over stdio, on
top of the client, so state and pinned host keys are shared.
