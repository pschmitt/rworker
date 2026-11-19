<div align="center">
  <img src="assets/rworker.svg" width="160" alt="rworker logo: a server stack whose bottom slab dissolves into particles, with a padlock">
  <p>
    <a href="https://nixbot.brkn.lol/repos/github/pschmitt/rworker"><img src="https://nixbot.brkn.lol/repos/github/pschmitt/rworker/badge.svg?branch=main" alt="nixbot"></a>
    <a href="https://github.com/pschmitt/rworker/actions/workflows/build.yaml"><img src="https://github.com/pschmitt/rworker/actions/workflows/build.yaml/badge.svg" alt="build"></a>
    <a href="https://github.com/pschmitt/rworker/actions/workflows/test.yaml"><img src="https://github.com/pschmitt/rworker/actions/workflows/test.yaml/badge.svg" alt="test"></a>
    <a href="https://github.com/pschmitt/rworker/actions/workflows/lint.yaml"><img src="https://github.com/pschmitt/rworker/actions/workflows/lint.yaml/badge.svg" alt="lint"></a>
  </p>
  <h1 align="center">rworker</h1>
  <p align="center">Disposable, encrypted build VMs on request: create one, build on it, throw it away.</p>
</div>

Heavy compiles (Rust, Android, large Nix builds) do not belong on a laptop.
rworker asks a control host for an ad-hoc OpenStack worker, pins its SSH host
key on creation, and wraps everything you do with it afterwards: shells,
file copies, background jobs and Nix remote builds. A worker deletes itself
when its time runs out.

## Features

- **One-shot builds**: `exec` creates a worker, copies directories to it,
  runs the command, copies results back and deletes the worker, also on
  errors and Ctrl-C.
- **Pinned host keys**: each worker's SSH host key is recorded at creation
  and pinned by worker name (`HostKeyAlias`), never trusted on first use.
- **Two overlays**: workers join NetBird and Tailscale; the client probes
  both addresses and uses the one that answers.
- **Background jobs**: `start`, `jobs`, `logs` (`-f` follows every worker,
  with lifecycle events) and `ps`, through a small job manager on the worker.
- **Nix builders**: `builder` prints a `--builders` line; `nix-cache` lets a
  worker substitute from this host's S3 cache over SSH only.
- **Live list**: `ls --watch` redraws in place and marks new, changed and
  gone workers; `--topic`, `--agent` and `--agent-id` filter by regex.
- **Scripting**: `--json` on every action but `ssh`; an MCP server
  (`rworker-mcp`) exposes the same actions to agents.
- **Shell completions**: zsh and bash, for actions, flags, worker names and
  the topics, agents and agent ids of the current workers, answered from a
  local cache so TAB never waits on the network.

## Install

```nix
{
  inputs.rworker.url = "github:pschmitt/rworker";
  # in your NixOS or Home Manager configuration:
  nixpkgs.overlays = [ inputs.rworker.overlays.default ];
  home.packages = [ pkgs.rworker ];
}
```

From source: `cargo install --path .` (needs OpenSSH and rsync at run
time). See [Nix](docs/nix.md) for the flake outputs and the MCP server.

## Quick start

```sh
export RWORKER_HOST=control.example.com              # the host that runs the control command
w=$(rworker up --ttl 3h --topic "release build" --json | jq -r .name)
rworker sync "$w" ./myproject                        # rsync to ~/myproject on the worker
rworker ssh "$w" 'cd myproject && cargo build --release'
rworker cp "$w":myproject/target/release/app ./app   # copy the result back
rworker down "$w"                                    # delete it
```

`rworker help` lists every action; `rworker help ACTION` explains one.

## Documentation

| Topic | |
| --- | --- |
| [Usage](docs/usage.md) | Actions, worker names, environment variables |
| [Nix](docs/nix.md) | Flake outputs, overlay options and the MCP server |
| [Development](docs/development.md) | Building, tests and the parity harness |
| [Parity contract](PARITY.md) | What the Rust client keeps from the bash client it replaces |
| [Changelog](CHANGELOG.md) | Release notes |

## License

GPL-3.0-only. See [LICENSE](LICENSE).
