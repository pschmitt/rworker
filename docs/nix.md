# Nix

## Flake outputs

```sh
nix build github:pschmitt/rworker               # the client with its completions
nix run github:pschmitt/rworker -- help
nix develop                                     # cargo, clippy, rustfmt, cargo-deny, the harness's tools
nix flake check                                 # unit tests, inventory API, MCP test
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

### Control-host inventory API

The control service refreshes inventory five seconds after each successful or
failed provider read. CLI and API readers share this snapshot. After a
control mutation or `rworker ls --fresh`, the refresher starts another
provider read. An unavailable or older-than-30-second snapshot returns an error.

The read API accepts `GET /v1/workers` and `GET /v1/workers/{name}`.
The responses are the worker array and a single worker object respectively.
Responses include an `ETag` for conditional reads and
`X-Rworker-Inventory-Age-Ms` for freshness. A missing worker returns 404.
A missing or invalidated snapshot returns 503 until the next successful refresh.

`GET /v1/workers/events/stream` sends the current worker array as a Server-Sent
Event followed by changed arrays. It uses the same authentication and snapshot
as the read API and keeps the connection alive between changes.

```sh
curl --unix-socket /run/rworker-inventory-api/http.sock http://localhost/v1/workers
```

To expose the API on a private network, set `services.rworker.api.https` with
`serverName`, `listenAddress`, `certificateFile`, `certificateKeyFile`, and
`clientCAFile`. The certificate paths refer to runtime files on the server.
Use a dedicated client CA whose certificates are issued only to API clients.
nginx verifies the client certificate and has access to the read API socket.
The TUI can use this HTTPS endpoint or its existing authenticated control connection.


## On-demand Nix builders

The NixOS module can provision workers on demand through a stable SSH gateway.
See [On-demand Nix builders](nix-builders.md) for gateway and client options.
