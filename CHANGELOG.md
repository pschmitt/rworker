# Changelog

## 0.1.0 (unreleased)

Rewritten in Rust from the bash client of nixos-config (`pkgs/rworker`).
Actions, flags, output, JSON and exit statuses are the same; see
[PARITY.md](PARITY.md) for the contract and the accepted differences.

### Control inventory and JSON output

- Share a control-host inventory snapshot across SSH and local clients.
  Use `rworker ls --fresh` to request a refresh before reading.
- Serve the snapshot at `GET /v1/workers` and `GET /v1/workers/{name}` via a
  group-restricted Unix socket, with ETags and snapshot age headers.

- Fetch detailed OpenStack inventory in one command and overlap it with
  the NetBird and Tailscale reads.
- Pretty-print JSON on a terminal with syntax colors. `NO_COLOR` disables
  colors while keeping indentation. Redirected and piped output keeps its
  existing format.

### New

- The TUI uses the same agent icon colors as `rworker ls`.

- `rworker tui`: an interactive terminal UI (k9s style). The worker list,
  refreshed in the background; the jobs of a worker; job logs, all logs
  and lifecycle events, followed live with scrolling and a line filter;
  ssh, inspect, ps, extend, topic, claim, down, up, prune, niks3-token,
  nix-cache and builder on the selected worker; `:` for any other action.

- Completions for the values of `--topic`, `--agent` and `--agent-id`: the
  topics, agents and agent ids of the current workers, with the workers
  that have them (`rworker logs -f --topic rwor<TAB>`). The filters of `ls`,
  `logs` and `events` get them as escaped regexes; a leading `!` stays.

- `rworker completions zsh|bash` prints the completion scripts, which are
  compiled into the binary; the Nix package installs them from there.

- Home Manager module `programs.rworker` (`homeModules.default`): control
  host, public key, login user, probe timeout, icons, SSH connection
  sharing, extra runtime inputs, the AWS credentials file for `nix-cache`,
  shell aliases and the MCP server, as defaults in the client's wrapper.
- `RWORKER_AWS_CREDENTIALS_FILE`: the AWS credentials file `nix-cache`
  reads, for rworker only.

### Fixed

- Job commands (`logs`, `jobs`, `ps`, `logs -f`) on a worker seen for the
  first time printed nothing: the control-host lookup swallowed the job
  manager script. `prune` with several workers deleted only the first.

### Control-host cutover safeguards

- `rworker server CONFIG reap --dry-run` reports deletion candidates and
  skips provider mutations. NixOS can select it with
  `services.rworker.reaper.dryRun`.
- Setting `reapProtectInUse` in the control settings preserves workers
  with an owner or topic and requires a successful idle probe before
  deleting other candidates.
