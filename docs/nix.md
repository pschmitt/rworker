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
- `homeModules.default` (also `homeManagerModules.default`): the
  [Home Manager module](#home-manager-module).

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

## Home Manager module

```nix
{
  imports = [ inputs.rworker.homeModules.default ];
  programs.rworker = {
    enable = true;
    controlHost = "control.example.com";
    publicKeyFile = "/home/alice/.ssh/id_ed25519.pub";
    nixCache.awsCredentialsFile = "/run/user/1000/secrets/nix-cache-aws";   # read-only key
    mcp.enable = true;
    aliases = [ "rk" ];
  };
}
```

The settings become defaults in the client's wrapper, so they apply
however it is started, and an environment variable set by hand still wins.
`programs.rworker.finalPackage` and `programs.rworker.mcp.finalPackage` are
the configured client and MCP server, for MCP client configurations. The
completions come with the package (`share/zsh/site-functions`,
`share/bash-completion`).

## MCP server

`rworker-mcp --rworker PATH` serves the actions as MCP tools over stdio, on
top of the client, so state and pinned host keys are shared.
