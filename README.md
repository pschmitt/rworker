<div align="center">
  <img src="assets/rworker.svg" width="160" alt="rworker logo: a server stack whose bottom slab dissolves into particles, with a padlock">
  <p>
    <a href="https://nixbot.brkn.lol/repos/github/pschmitt/rworker"><img src="https://nixbot.brkn.lol/repos/github/pschmitt/rworker/badge.svg?branch=main" alt="nixbot"></a>
    <a href="https://github.com/pschmitt/rworker/actions/workflows/build.yaml"><img src="https://github.com/pschmitt/rworker/actions/workflows/build.yaml/badge.svg" alt="build"></a>
    <a href="https://github.com/pschmitt/rworker/actions/workflows/test.yaml"><img src="https://github.com/pschmitt/rworker/actions/workflows/test.yaml/badge.svg" alt="test"></a>
    <a href="https://github.com/pschmitt/rworker/actions/workflows/lint.yaml"><img src="https://github.com/pschmitt/rworker/actions/workflows/lint.yaml/badge.svg" alt="lint"></a>
    <a href="https://my.home-assistant.io/redirect/marketplace_repository/?owner=pschmitt&amp;repository=rworker&amp;category=integration"><img src="https://my.home-assistant.io/badges/marketplace_repository.svg" alt="Install the Home Assistant integration"></a>
  </p>
  <h1 align="center">rworker</h1>
  <p align="center">Disposable build workers on OpenStack, GitHub Actions, and GitLab CI.</p>
</div>

Heavy compiles (Rust, Android, large Nix builds) do not belong on a laptop.
rworker asks a control host for a time-limited worker and pins its SSH host
key on creation. Use it for shells, file copies, background jobs, and Nix
remote builds.

## Features

- **GitHub Actions workers** offer native Linux ARM64 and x86-64 with
  interactive SSH and Nix. See [GitHub Actions workers](docs/github-actions.md).
- **GitLab CI workers** lease an isolated Linux x86-64 CI job with SSH and
  Nix. See [GitLab CI workers](docs/gitlab-ci.md).
- **One-shot builds**: `exec` creates a worker, copies directories to it,
  runs the command, copies results back and deletes the worker, also on
  errors and Ctrl-C.
- **Pinned host keys**: each worker's SSH host key is recorded at creation
  and pinned by worker name (`HostKeyAlias`), never trusted on first use.
- **Two overlays**: workers join NetBird and Tailscale; the client probes
  both addresses and uses the one that answers.
- **Background jobs**: `start`, `jobs`, `logs` (`-f` follows every worker,
  with lifecycle events) and `ps`, through a small job manager on the worker.
- **Nix builders**: `builder` prints a `--builders` line; `setup nix` gets a
  worker ready over SSH only: it substitutes from this host's S3 cache and
  fetches private flake inputs with this host's access-tokens (`up` and
  `exec` do it by default; `--no-cache` / `--no-auth` leave one out).
  `setup` is the place for more of this kind of preparation.
- **Live list**: `ls --watch` redraws in place and marks new, changed and
  gone workers; `--topic`, `--agent` and `--agent-id` filter by regex.
- **On-demand Nix builders** lease a worker through a stable SSH endpoint. See [On-demand Nix builders](docs/nix-builders.md).
- **Terminal UI**: `tui` shows the workers, their jobs and logs, followed
  live, and runs the actions on the selected worker with a key or two.
- **Home Assistant** receives worker events and fleet counts over WebSocket
  and exposes worker actions. See [Home Assistant](docs/home-assistant.md).
- **Scripting**: `--json` on every action but `ssh`; an MCP server
  (`rworker-mcp`) exposes the same actions to agents.
- **Shell completions**: zsh and bash, for actions, flags, worker names and
  the topics, agents and agent ids of the current workers, answered from a
  local cache so TAB never waits on the network.

## Install

```nix
{
  inputs.rworker.url = "github:pschmitt/rworker";
  # in your Home Manager configuration:
  imports = [ inputs.rworker.homeModules.default ];
  programs.rworker = {
    enable = true;
    controlHost = "control.example.com";
  };
}
```

From source: `cargo install --path .` (needs OpenSSH and rsync at run
time). See [Nix](docs/nix.md) for the module's options, the overlay and the MCP
server.

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
| [GitHub Actions workers](docs/github-actions.md) | Controller setup, hosted runner profiles, and SSH leases |
| [GitLab CI workers](docs/gitlab-ci.md) | Runner requirements, pipeline authentication, and SSH leases |
| [OCI client image](docs/oci-client.md) | Run the rworker client from a CI job |
| [Nix](docs/nix.md) | Flake outputs, overlay options, the web console and MCP server |
| [On-demand Nix builders](docs/nix-builders.md) | Gateway, machines entry and worker leases |
| [Worker images](docs/images.md) | Building, uploading and booting worker images |
| [Home Assistant](docs/home-assistant.md) | Integration setup, entities, diagnostics and actions |
| [Development](docs/development.md) | Building and testing |
| [Changelog](CHANGELOG.md) | Release notes |

## License

GPL-3.0-only. See [LICENSE](LICENSE).
