# Changelog

## 0.1.0 (unreleased)

- Accept `topic:`, `agent:` and `agent-id:` search terms in `rworker ls` and its aliases.

- Add highlighted `topic:`, `agent:` and `agent-id:` searches to the web log viewer, alongside line-text filtering.

- Accept `topic:`, `agent:` and `agent-id:` search terms in `rworker logs`.

- Exclude worker and job prefixes when selecting log text in the web console.

- Show jobs from the operator’s GitHub Actions and GitLab CI leases in the web UI and cached job list.

- Authorize the web console on pool workers created by the legacy autoscaler, so their jobs appear in the web UI.

- Add multiple SSH public keys in the TUI and web worker creation forms.

- Add validated additional SSH public keys when creating workers from the CLI, TUI or web UI.

- GitHub Actions and GitLab CI workers record the same creator metadata as OpenStack workers, including the Concord agent ID when supplied.

- Double-clicking a row in the TUI's tables (workers, jobs, profiles) acts like pressing Enter.

- The TUI's list of one worker's jobs includes finished jobs by default; the list of all workers' jobs still shows only active ones. `d` toggles both.

- Every worker has one login account, `rworker` (with a group of the same name), instead of `agent` on OpenStack workers and `rworker-build` on GitHub and GitLab leases. NixOS worker images get it from `nixosModules.workerImage` (`nix/worker/account.nix`, which also sets `rworker.worker.jobMembers`) and publish it as the Glance property `rworker_login_user`; a lease's agent reports it with its endpoint. The controller records each worker's `login_user`, and the client, log collector, builder gateway and `setup nix` use it; workers that report none keep their old account. Jobs move to the shared `/var/lib/rworker/jobs` (`RWORKER_JOBS_DIR`, setgid, group `rworker`), falling back to `~/.local/state/rworker/jobs` on older workers.

- Workers leased by one of the operator's devices (owner `OPERATOR@DEVICE`, for example a phone's `nixpp` build) also accept the operator's client keys from the server module's `services.rworker.operatorKeys` (`owner`, `publicKeys`), restricted like the console key. `jobs`, `logs` and `ssh` from any of the operator's workstations then reach them; they failed with "Could not list jobs" before.

- `ssh`, `sync`, `cp` and `start` accept `--wait` to wait for a booting worker and retry SSH until it is reachable, with an optional timeout in seconds.

- The NixOS server module can mint the GitHub Actions credential itself: with `services.rworker.githubActions.app` (`idFile`, `privateKeyFile`, `repository`) it installs `rworker-github-token.service` and its timer, which exchange a GitHub App key for a repository-scoped `actions: write` installation token every 20 minutes and retry failures every 30 s. Set the runtime JSON's `githubActions.tokenFile` to `app.tokenFile`. See [GitHub Actions workers](docs/github-actions.md#authentication).
The Rust client and controller replace the original Bash client.

- CI leases (GitHub Actions and GitLab) accept SSH agent forwarding (`ssh -A`), so a build there can use the client's key without the key being copied to the runner. TCP, Unix socket and X11 forwarding and tunnels stay off.
- The pool autoscaler no longer deletes a draining worker that has new work: it asks Circus for the worker's assigned jobs right before the deletion, and returns the worker to the pool when it is busy or builds are queued. When Circus does not answer, the worker stays.
- The builder gateway names the topic of the workers it leases after the client host and profile ("Nix builder for fnuc (standard)") instead of a fixed "On-demand Nix builder".
- The web console's phone layout takes two header rows instead of three: the search box sits next to the view title with settings at the right edge (also in views without search), and the filter chips scroll sideways in one row behind a compact sort button.
- `jobs` without a worker (also `--watch` and the TUI's jobs view) reads the control host's job cache, which the job collector refreshes every 3 seconds, instead of asking every worker over SSH and waiting for the slowest (about 30 s with an unreachable worker). It asks the workers itself when the cache is missing, older than 30 s or kept for another user; `--fresh` always does. The web console's jobs come from the same cache instead of a poll of its own.
- `logs --follow` in a terminal opens the TUI with just that log and no chrome (snapshots without `--follow` print and exit as before): the output looks the same (long lines wrap; w toggles), q quits and leaves the log printed in the terminal, W or Esc switches to the worker list, J to the jobs, / filters and n toggles the chrome. `--plain` prints the plain stream as before, which is also what pipes, scripts and `--json` get.
- In the TUI's log and output views, J opens all jobs when the view is not about one worker (for example the log of all workers); it did nothing there.
- The web console shows new events (workers created or deleted, jobs started, failed or finished) as toasts in the bottom right corner, with a button to open the worker or job.
- The login of `ssh --create` and `up --ssh` shows up as an `ssh` job with the worker's topic, like `exec`'s job, and the session is recorded in the job's log where the worker has script(1).
- `ssh --create` and `--rm` work anywhere among the options (`ssh --profile gh-aarch64 --create --rm`); `--create` had to come first.
- The web console's compact density also tightens the cards and console layouts, events, profiles, pools, worker facts and the phone list; it only changed the split list and jobs list before.
- The web console's sidebar button for the command palette is labeled "Palette" instead of "Jump to…".
- Fix scrolling in the web console's Logs view: the log grew to its full length and was cut off instead of scrolling.
- On phones, swiping up on the web console's tab bar opens the command palette, and pulling the palette down closes it. The palette grows with its content up to almost the full screen height (its list stopped at 460px) and no longer shows an empty area below a short list.
- On phones, the web console's search is a magnifier button next to settings; tapping it opens the search box across the header, and it closes again when it is left empty.
- The web console's Logs view filters lines with the search box in the header, like the other views, instead of a separate box in the log tools (which phones did not show).
- The web console's log viewers show the live state (Live, Snapshot, Archived) as a pill floating in the log's top right corner instead of in the log tools.
- On phones, the web console shows the New worker button only in the Workers view.
- The web console no longer underlines worker names in the cards and in the expiry banner.
- NetBird is optional: leave `netbirdApi` unset to run without it. When its API fails, a lease enrolls the worker with Tailscale alone (it fails only if no overlay can enroll it) and waits for it to join, CI leases join the overlay that works, `ls` keeps the last known addresses, the reaper keeps workers whose activity it cannot determine, and one unreachable worker no longer stops a reaper or autoscaler pass. Deleting a worker no longer fails because its NetBird peer cannot be removed.
- Worker SSH sessions send keepalives (every 15 s, eight misses allowed), so a long silent build from a phone is no longer reset by a NAT or mesh path.
- Show the topic in the TUI's jobs view on narrower terminals too (the start time goes first), and the worker's topic, dimmed, for jobs without one.
- Add a topic to jobs: `start --topic`, `setup register-log-file --topic` (MCP `topic` on `start`, `exec` and `setup_register_log_file`; `exec` workers give the job their own topic). `jobs`, the TUI and the web console show it as a column, `jobs --json` as `topic`; tab-separated output has it last. `start` without `--topic` warns. The web console falls back to the worker's topic.
- Keep pool autoscalers and worker SSH working over Tailscale while the NetBird API is down: reaching a worker Tailscale knows and checking the group membership no longer abort a reconcile. New workers still need a NetBird setup key.
- Set up GitHub Actions and GitLab CI leases like OpenStack workers on `up`, `exec` and `ssh --create`: the S3 Nix cache, the client's Nix access-tokens and a niks3 push token, as far as the client host has them (`--no-nix-cache`, `--no-nix-auth`, `--no-niks3-token`; MCP `nix_cache`, `nix_auth`, `niks3_token`). This replaces the rule that CI workers receive no private Nix credentials: they now sit on the shared runner, so keep them off with the flags below. The files live in the build user's home, the runner's SSH sessions (and `rworker-nix-daemon`) read them, and `~/.local/bin/niks3` pushes with the lease's token. Ubuntu's Nix on `gl-x86_64` cannot open `s3://` stores: the cache is skipped there with a warning (use `gl-x86_64-lease`). These runner changes apply from `main`.
- Make `setup nix`, `setup nix cache`, `setup nix auth` and `setup niks3-token` work on CI leases; they reported "image-too-old" or "no worker". The controller hands the lease's token to the client, which installs it over SSH (it cannot log in to a lease itself); a lease cannot be extended, so the token lasts as long as the lease.
- Add `--no-credentials` to `up`, `exec` and `ssh --create` (MCP `credentials=false`): the same as `--no-nix-cache --no-nix-auth --no-niks3-token`.
- Bring MCP tools up to date with worker provisioning, file copy and blob uploads, one-shot execution, hosted builder setup, image operations, and bounded inventory, job, log and event watches.
- Group Nix worker setup under `setup nix`, with `cache`, `auth`, `builder`, and `upload-blob` for local fixed-output sources. The existing `nix-cache` and `nix-builder` spellings still work.

- Ignore a deleted worker that OpenStack lists once more as DELETING: the event history recorded it as added and deleted a second time, without the deletion reason.
- Leave builder gateway workers out of job polling and `logs -f`, and use the console's key only on pool workers: refused logins got the controller penalized by the workers' sshd, and the gateway then deleted healthy leases within minutes.
- Log why the Home Assistant integration's inventory stream failed, once per outage.
- Keep the inventory cache files group-readable whatever the writer's umask: the builder gateway's 0077 hid the inventory from the web and automation API, which then streamed no snapshots (Home Assistant stayed unavailable).
- Refuse a builder gateway `authorizedKeysFile` in the Nix store: sshd's StrictModes rejects it, so every gateway login failed. The docs show a copied `/etc` file.
- Record who deleted a worker and why (the reaper's reason, `down` by its caller, the builder gateway, the autoscaler, a failed `up`) as the details of its `worker-deleted` event.
- Open an event's details in the web console by clicking or tapping it; deletion events show who deleted the worker and why.
- Show the log of the worker panel and the Logs view full screen (button in the log tools, Esc leaves).
- Fix the web console's Profiles list on phones, which lost the profile names and details.
- Order the web console's views Workers, Jobs, Logs, Profiles, Events (keys 1 to 5 follow).
- Lay out the web console's Profiles view by the layout setting too: a list (split), a sortable table (console) or cards; a click opens the profile details.
- Lay out the web console's Jobs view like the Workers view: split (compact list beside the job's log), console (sortable table above it) or cards. Opening a job keeps you on Jobs (`#jobs/WORKER/JOB`), J/K move between jobs and Esc closes the job.
- Show each job's worker topic and agent in the web console's Jobs list, under a sticky header row, and match them in its filter.
- Show live registered jobs with an amber activity icon and pill (like running jobs and the sidebar's count) instead of a grey clock, and archived jobs muted (grey icon and name, outlined pill) in the web console.
- Collapse the web console's sidebar to an icon rail (button at its foot, or `[`), kept in the browser; the collapsed Jobs icon shows an amber dot while jobs run.
- Resize the columns of the web console's worker table by dragging the header dividers (arrow keys when focused, double-click resets); widths are kept in the browser.
- Show only the number of running jobs on a worker's Jobs tab, amber like the sidebar's count, and nothing when no job is running.
- Move `builder` and `nix-cache` under `setup`: `setup nix-builder` and `setup nix-cache`; the old top-level actions are gone. Without a worker name, `setup nix-builder` prints a line for the on-demand builder gateway (`--profile`, `--arch`, `--pool`, `--gateway`, `--identity`, `--host-key`, `--system`, `--jobs`, `--features`; Home Manager `programs.rworker.builderGateway`). The MCP tool `builder` becomes `setup_nix_builder` with the gateway selectors, and `nix_cache` is gone (use `setup_nix` with `auth=false`).
- Pick the builder gateway's worker per machine entry: `?remote-program=profile:NAME`, `arch:ARCH` or `pool:NAME` leases a worker of that profile, architecture or pool, each with its own lease and idle timer; `arch:aarch64` and `profile:gh-aarch64` lease a GitHub runner and build in its isolated store. The NixOS client module gains `selector` and `extraMachines`.
- Mark workers created in the last five minutes with a "new" badge in the web console.
- Flash web console worker rows only when their status, topic, agent or lease state changes, not on every lease countdown tick.
- Keep retrying the TUI's log preview of a worker that has no IP yet, so its logs appear once it is reachable.
- Push job changes: the job event collector also covers pool workers (with the console's key), the web console's update socket sends "jobs" on every job event, and `jobs --watch` and the TUI refresh on the controller's job events instead of waiting for the next poll.
- Recover workers whose creation was cut short: the reaper pins the host key from the boot console instead of deleting the worker as unready.
- Refresh the web console's job counts every 10 seconds on every page, so the sidebar and the worker Jobs tab match the Jobs page.
- Replace the web console's selects with searchable dropdowns that show worker topics, agents and leases and job states; the Logs view's job picker lists every job when no worker is chosen.
- Drag the divider between the worker list and the open worker in the web console's split and console layouts to resize them (double-click resets).
- Add `logs --filter REGEX` (`--grep`, `-g`, repeatable): only lines matching any of the case-insensitive expressions, matches highlighted and the lines' colors kept, also with `--follow`.
- Poll jobs for lifecycle events only on workers that carry the log collector's key: logins refused on pool workers got the controller's address penalized by sshd, which made the web console's job list lose workers.
- Show job states with icons in the web console, including an archive icon for archived jobs.
- Keep the colors of redacted logs in the web console, so combined logs show the colored worker/job prefixes.
- Make the web console an installable app (PWA): manifest with app icons and shortcuts, theme colors, and a service worker that keeps the shell for offline starts without caching console data.
- Add `ssh -i FILE` (`--identity`) to log in to a worker with a specific private key.
- Log in to pool workers as `agent`, like the web console, so jobs started from the CLI and the console are the same jobs.
- List jobs of pool workers in the web console: it polls reachable workers alongside the log collector, and autoscalers authorize the console's restricted SSH key on new pool workers (`autoscalers.<name>.webConsoleAccess`).
- Mark the current web console view with a filled background and accent icon instead of a side bar.
- Sort the console layout's worker table by clicking a column header.
- Show agent icons in the console layout's worker table.
- Show why a web console action failed: the controller's error message, with secrets masked, instead of a generic error.
- Add a console layout to the web console: a dense worker table with the selected worker's logs below and a shortcut bar.
- Show the new expiry when choosing a lifetime for a new or extended worker.
- Show ANSI colors in the web log viewer, such as the colored worker/job prefixes of combined logs.
- Add right-click menus for workers and jobs in the web console, and kind icons in the console layout.
- Wrap the worker actions below the name in narrow detail panes instead of drawing them over it.
- Fix web console jobs, logs, processes and job starts on GitHub Actions workers: the console's commands now use the controller's local relay and the runner's login, like its terminal.
- Add a special-keys bar (Esc, Tab, sticky Ctrl, ^C, ^L, ^U, arrows, ...) and a capture-all-keys mode (full screen with keyboard lock) to the web SSH terminal.
- Wrap the web console's filter chips on phones instead of cutting them off, and let the SSH terminal fill its sheet.
- Redesign the web console: a sidebar with a split view (worker list beside the open worker), one icon set and button style, a full-height log viewer with job picker, line filter, follow, wrap, copy and download, toasts instead of banners, a non-blocking new-worker drawer with lifetime presets, an expiry banner with one-click extension, a command palette (Ctrl+K) and keyboard shortcuts, and a phone layout with bottom tabs and sheets.
- Add console settings for a cards layout with full worker pages, a light theme and a compact density.
- Replace the logo with a hard-hat ant and show it beside the name in the README, with a rimmed variant for dark mode.
- Use the hard-hat ant as the web console favicon and header mark, and show it on the sign-in card.
- Use the hard-hat ant for the Home Assistant integration icon and logo, with dark-theme variants, rendered by `scripts/brand-icons.sh`.
- Show profile architecture in JSON, CLI, TUI and web profile listings.
- Try profiles matching an architecture with `--arch`.
- Include the native system and SSH relay configuration in hosted builder JSON.
- Add a hosted queue timeout and report exit 75 for unavailable architecture pools.
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
