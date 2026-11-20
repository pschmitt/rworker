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

- Offer an opt-in NixOS SSH gateway and machines entry for on-demand Nix builds.

- Store and display the OpenStack image name and ID in worker inspection.

- Push worker inventory to the TUI and show workers as they are building.
  The TUI can also show or hide retired workers inline with `h`.

- Open the TUI when running `rworker` without arguments in a terminal.
  Use `Alt+Shift+Q` to quit from any view.

- Add `jobs --watch` and aligned, colored terminal tables for jobs, profiles,
  and processes. Interactive list watches update only changed rows.
- List workers with `machines`, `ls`, `nodes`, or `instances`. Accept unique
  action prefixes, `info` for `inspect`, and `flavors` for `profiles`.


- Archive one-shot `exec` stdout and stderr before worker cleanup. The TUI
  has a retired-worker history and a borderless Jobs view with active jobs
  shown by default.


- Collect registered job logs on the control host under the operator's account.
  Read retained snapshots with `logs WORKER [JOB] --cached` and redact
  matching text on the client with `--redact REGEX`.

- List jobs across reachable workers with `rworker jobs`. In the TUI,
  press `j` for the all-worker jobs view or Enter for the selected worker.

- Align the `ls` table with the TUI. Use `--wide` or `-o wide` for owner,
  profile and address columns.
- Follow the selected worker’s logs beside or below the borderless worker
  list. Use the mouse to select rows and scroll tables and logs.

- Add OpenStack cloud and project selectors to image uploads.
- Ship a standalone NixOS worker image and a reusable image module.

- Build worker disk images with `rworker image create` and upload private
  images to OpenStack with `rworker image upload`.

- Remove the worker row markers from the TUI and the `KIND` column from
  the TUI and `ls` tables. The JSON schema still includes `kind`.

- The TUI moves worker details beside the table on wide terminals and
  prioritizes essential columns on narrow ones.

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
