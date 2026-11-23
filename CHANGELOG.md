# Changelog

## 0.1.0 (unreleased)

The Rust client and controller replace the original Bash client.

- Show profile architecture in JSON, CLI, TUI and web profile listings.
- Try profiles matching an architecture with `--arch`.
- Label the profile tag column “Tags” throughout the interfaces.

- Show profile and agent icons and colors on worker creation and deletion events.

- Add on-demand setup and repair for ephemeral NetBird and Tailscale on CI workers.

- Move manual niks3 token reissue and existing log registration under `setup`, with matching MCP tools.

- Use rworker's icon colors in Zsh completion menus.

- Show profile, worker, and agent icons in Zsh completion menus while preserving the inserted values.

- Add optional ephemeral Tailscale and NetBird proxies for CI workers.
- Include a sample flake for building a custom OpenStack worker image.

- Verify new CI smoke-test files and Nix store registrations on the LUKS2 backing filesystem.

- Add a no-lease CI benchmark for encrypted and unencrypted storage and Nix builds.

- Encrypt GitHub Actions and GitLab CI lease storage by default with a disposable LUKS2 key.

- Report storage device access and filesystem types in GitLab runtime checks.

- Keep CI run and job identifiers in worker metadata for GitHub and GitLab.

- Use eight-character suffixes for new GitHub and GitLab worker names.

- Label the web profile-row create action “Worker”.

- Use the standard close icon in the web SSH dialog.
- Open the new worker form from a profile row in the web console or the TUI.

- Fix HTTPS trust in lease SSH sessions and bundle curl and OpenSSL.

- Refresh the web console font so browsers show the GitLab glyph.
- Publish a Nix-built lease image and use it with `gl-x86_64-lease`.

- Keep profile listing fast when provider data is missing or unavailable.
- Add a `gitlab` tag to GitLab profiles.
- Show the GitLab Nerd Font glyph in GitLab orange for workers and profiles.

- Lease regular GitLab CI jobs with the `gl-x86_64` worker profile.
- Show the GitLab CI profile immediately when it is configured.

- Publish a Nix-built rworker client image to GHCR for amd64 and arm64.

- Link to GitHub Actions worker setup from the README and document hosted
  runner profile selection and lease expiry in the usage guide.

- Show GitHub and OpenStack profile glyphs in the CLI, TUI and web console.

- Use GitHub black and OpenStack red for profile icons in the web console.

- Use provider colors on the terminal background in `rworker profiles`.

- Show profile RAM and disk in decimal GB across providers in the CLI.

- Open any standalone TUI view by name or prefix, such as `rworker tui prof`.

- Show profile image and runner details in the web console, TUI, and
  `rworker profiles -o wide`.

- Serve profile image details from the controller's background cache.

- Select a profile in the TUI and press Enter to see its details.

- Expose OpenStack overrides and an option to open SSH after creation in the
  web console's New worker dialog.

- Show SSH process failures in the web terminal status.

- Give web worker actions an icon toolbar and clarify process lookup failures.
- Render provider glyphs directly on the dark web console surfaces.
- Put the worker details back chevron beside the worker name.
- Add an exit icon to the web console's Sign out link.

- Add `rworker ssh --create --rm` to delete the new worker when SSH exits.

### GitHub Actions

- Keep SSH sessions alive across brief controller API interruptions.

- Apply the Nix stack limit through PAM for hosted worker logins.

- Use the standard worker creation spinner for GitHub queues and SSH startup.

- Cache profile lookups for GitHub runners and display only the observed
  total disk size.

- Give SSH sessions their own Nix configuration path and enough stack for
  Nix evaluation on GitHub runners.

- Report the latest observed GitHub runner disk size and free space at startup.

- Show GitHub queue and SSH startup progress with a clickable workflow link,
  report workflow failures, and retry brief controller interruptions.

- List `gh-aarch64` and `gh-x86_64` with the `github-actions` tag and show the
  standard worker summary when a GitHub lease is ready.

- Complete the GitHub runner profiles in shell commands.

- Show GitHub’s published CPU, RAM, and disk allocations in the worker profile list.

- Keep one-shot build logs after the GitHub worker closes when controller log archiving is enabled.

- Show the GitHub glyph for Actions workers in the CLI, TUI, and web inventory.

- Add a native Linux runner workflow and agent bootstrap for controller-assigned
  build jobs, with KVM enabled where the runner supports it.

### TUI

- Restore the terminal cursor when handing off to SSH or exiting the TUI.

- Show live SSH worker provisioning stages and elapsed time in the footer.

- Provision SSH workers in the background with a status spinner, then open SSH when ready.

- Keep worker status under the table when logs are shown alongside it.

- Move status to the bottom and show shortcuts in help for the current view.

- Add a worker creation form with profile resources and SSH options.

- Use one divider around the worker log preview and remove the log spinner.

- Remove the live inventory badge and give the stacked log pane one more row.

- Show pending down requests in the worker status column and display the full
  `DELETING` status in bold red.

- Toggle chrome in every TUI view with `n` and start a job with `c`.

- Use `c` to create a worker and `C` to claim one.

- Press `n` in logs or other text views to show command output without TUI chrome.
- Explain when the worker log preview is empty because it has no jobs.
- Open a worker's only active job directly; use `J` in its logs to show jobs.
- Toggle log following with Space and clarify the navigation hints.

### Home Assistant

- Add icons to worker counts, inventory diagnostics and worker lifecycle events.

- Add a push integration with worker lifecycle events, fleet counts, diagnostics,
  and controller actions.

### Web console

- Keep profile badges compact and beside the profile name on mobile.

- Authorize the controller’s console key when provisioning workers owned by the web operator.

- Keep the console running when a browser SSH terminal opens or closes.

- Open an interactive SSH terminal from a live worker’s detail view.

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
