# Changelog

## 0.1.0 (unreleased)

The Rust client and controller replace the original Bash client.

### TUI

- Open a worker's only active job directly; use `J` in its logs to show jobs.
- Toggle log following with Space and clarify the navigation hints.

### Web console

- Cache profile flavor resources on the controller and refresh them daily;
  show CPU and memory in the CLI and TUI profile list too.
- Show profile vCPU counts and RAM from the configured OpenStack flavors,
  alongside disk sizes and image tags.
- Move the search icon inside the right edge of the filter field.

- Filter worker inventory directly from the live, pool, expiring and retired
  counts. The console keeps connection status in the header and makes worker
  links easier to scan and open.

### Worker provisioning

- Profiles can match image names in addition to Glance tags, so multiple
  image families can safely share a tag.

### Reaper

- Delete a worker that never became ready (no host key after the boot
  timeout) even with `reapProtectInUse`. Nobody can hold a pinned session on
  it and it cannot be probed, so it used to be kept for ever ("activity could
  not be ruled out").

### `setup`

- Add `rworker setup TOPIC NAME` to get a worker ready for a kind of work.
  The first topic, `nix`, pushes this host's S3 cache settings (what
  `nix-cache` did) and, new, its Nix `access-tokens`, so the worker fetches
  private flake inputs itself. `--no-cache`, `--no-auth` and `--auth-host`
  choose the parts; `nix-cache` stays as `setup nix --no-auth`.
- `up` and `exec` push both by default, as far as this host has something to
  give; `--no-nix-auth` keeps the tokens off the worker. The result's
  `nix_cache` object gains `auth` and `auth_hosts` (host names only). When
  the S3 profile is missing, `setup nix` and `up` still push the tokens.
- MCP: new `setup_nix` tool and `up(nix_auth=...)`.

### MCP

- Inspect job processes and retained lifecycle events.
- Read logs across workers or from retained snapshots, with optional secret
  redaction, and request fresh inventory or include retired workers.

- Show the Circus tent icon in the agent column on the CLI, TUI, and web.

### Web console

- Open worker details and logs on a full-width, linkable worker page.
- Connect to live logs immediately and use current log snapshots during reconnects.

### Autoscaler pools other than nixbot's

- Scale a pool on Circus CI demand (`demand.source = "circus"`: pending
  builds per system from the queue-runner's `/api/v1/operator/queue`, API key
  read from a file and passed to curl on stdin).
- Run a pool without a Nix machines file, choose its worker `profile`, label
  its workers (`poolLabels`) and hand them secrets through `userDataFiles`,
  read when a worker is created.
- Name the autoscaler unit (`services.rworker.autoscaler.unitName`), so a
  host can run such a pool next to or instead of nixbot's.

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

- Add a responsive, embedded web console for workers, jobs, events, profiles,
  and operator controls.

- Retain observed worker and job lifecycle events on the controller and follow
  them with `events -w` or the TUI.

- Preserve terminal colors in the redacted log view.

- Register running `exec` commands as live jobs named `exec`.

- Resize the TUI logs pane by dragging its border next to the worker list.

- Remove the legacy Bash client and its differential test harness.

- Color error usage consistently with normal help.
- Mask recognizable secrets in the local log view with bare `--redact`. Use
  `--redact-regex` for an additional pattern.

- Reuse restricted SSH connections when collecting worker logs.
- Escape brackets in zsh option descriptions so log completions load.

- Accept grouped short options such as `ls -Aw` and attached option values.

- Offer an opt-in NixOS SSH gateway and machines entry for on-demand Nix builds.
  Record the initiating machine, preserve retired builders in history and archive
  their Nix derivation logs.
- Include retired workers in the machine list with `--all`, `-A` or `--history`.
- Fall back to archived worker logs when a live log snapshot is unavailable.

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
- Setting `reapProtectInUse` in the control settings requires a successful
  idle probe before deleting a worker, including at maximum lifetime.
  The probe checks common build processes, Nix build users and one-minute
  load. Idle shells and log followers do not block cleanup.
