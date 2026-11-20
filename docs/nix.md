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


## Web console

For an independently configured HTTPS reverse proxy, set `web.origin` to its
exact public origin and proxy the Unix socket with authentication on all routes.
To reuse the proxy session for one operator, set `web.proxyUser`. The proxy must
strip any incoming `Remote-User` header and replace it with the identity from its
own authentication check. Only the proxy and trusted local accounts should have
access to the Unix socket.
Set `web.identityFile` and `web.publicKeyFile` to the matching SSH key pair when
the operator needs a dedicated identity.

Standard builds include the console through the default Cargo feature `web`.
To omit it, build with `cargo build --no-default-features` or use the Nix package
override `pkgs.rworker.override { web = false; }`.

Enable the console on a controller that already has `services.rworker.api.https`
configured. The browser uses the same HTTPS listener and client certificate as
the inventory API. The operator console also requires a separate access token.

```nix
services.rworker.web = {
  enable = true;
  operator = "operator";
  tokenFile = "/run/secrets/rworker-web-token";
  publicKeyFile = "/run/secrets/rworker-operator.pub";
};
```

Generate a random token, for example with `openssl rand -hex 32`, and store it
in the runtime file. Make the file readable by the configured operator. That
account needs a working SSH identity for the workers it manages. All console
actions run with the account's existing rworker permissions.

Open the configured HTTPS hostname in a browser with its client certificate,
then enter the operator token. The console provides workers and history, job
management, profiles, events, inspection, process lists, and log snapshots with
follow and optional redaction. It can copy an SSH command or a Nix builder entry.
Use the CLI for local file transfers, image workflows, interactive SSH, and
one-shot commands that need terminal I/O.

The HTML, styles and scripts are embedded in the rworker binary and the browser
loads them from the controller.
