# Usage

`rworker help` prints the overview and `rworker help ACTION` (or
`rworker ACTION --help`) the details of one action; this page explains the
concepts they share.

## The control host

rworker does not talk to OpenStack itself. It runs `rworker ACTION ...` on
the control host over SSH (one shared connection, kept for ten minutes) and
keeps what it learns in `$XDG_STATE_HOME/rworker` (default
`~/.local/state/rworker`): one state file per worker, the pinned host keys
(`known_hosts`) and the last `ls` answer for the completions.

## Worker lifecycle

Use `rworker profiles` to list the configured profiles. Select one with
`--profile` on `up`, `exec`, or `ssh --create`.

- `up` creates an ad-hoc worker and waits until it answers (usually 1-4
  minutes); `--ssh` logs in right away, `--json` prints it for scripts.
- `extend NAME --ttl 2h` moves the expiry; `down NAME` deletes it; `prune`
  deletes several (by default your own).
- An ad-hoc OpenStack worker still in use when it expires stays until it
  has been idle for an hour, up to its maximum lifetime.
- Always pass `--topic` (what it is for) and `--agent-id` when you create
  one; `claim NAME` takes a worker over when you reuse it.

[GitHub Actions workers](github-actions.md) use the `gh-aarch64` and
`gh-x86_64` profiles. Their default lease is 30 minutes, including time
spent in the GitHub Actions queue. At expiry the controller closes the
lease and cancels the workflow run. GitHub Actions workers skip the
automatic Nix cache credentials and private input tokens copied to
OpenStack workers.

## Worker names

A name may be abbreviated: an exact name wins, then a unique prefix, then a
unique substring (`rworker ssh 3e19`). An ambiguous fragment lists the
matches. `USER@NAME` picks the login user; the default is `agent` on ad-hoc
workers this host created and yourself on all others. Workers this host has
not seen yet are looked up on the control host on first use.

## Worker kinds and pools

The control host tells workers apart by their OpenStack metadata, not their
names: `nixbot-worker=adhoc` marks an ad-hoc worker, `rworker-pool=NAME` the
pool of an autoscaled one (each autoscaler sets it at creation, from
`poolName` or its `poolLabels.kind`). `ls` shows `adhoc` or the pool name as
KIND (`-o wide`); the reaper only ever deletes ad-hoc workers. Workers from
before this metadata fall back to their name prefix: `adhocPrefix`,
`poolPrefix` (nixbot's pool) and the optional `poolPrefixes` map
(prefix -> pool name).

Each worker shows the Nerd Font glyph of its image: the Glance image
property `rworker_icon` as a codepoint (`U+F0508`; Glance stores no 4-byte
UTF-8) and `rworker_label`, a display name, both set when the image is
published; without them, a glyph for its kind. JSON output carries
the kind, pool and label, never the glyph.

`rworker machines` lists workers and accepts the aliases `ls`, `nodes`, and
`instances`. Unique prefixes work too, for example `rworker lo` for `logs`
and `rworker no` for `nodes`. `rworker info` is an alias for `inspect`, and `flavors` is an alias for `profiles`.

## Jobs

`start NAME --name JOB -- 'COMMAND'` runs a command in the background on the
worker, `jobs NAME` lists them with their state, `logs NAME [JOB]` prints
their output and `logs -f` follows every job on every worker, with
lifecycle events and a periodic worker count on stderr.

Use `rworker jobs -w` to refresh the jobs table every five seconds, or
`rworker jobs WORKER -w -n 2` for one worker at a two-second interval.
Press `q` to quit or any other key to refresh immediately. With
`--json --watch` and redirected output, each refresh writes one JSON array
on its own line.

A control host can collect the operator’s registered logs every 30 seconds.
Set `services.rworker.logCache.operator` to the Unix account that already
has noninteractive SSH access to the workers. Set
`services.rworker.logCache.workerUser = "agent"` when collecting jobs run
as `agent` by clients on other hosts. The service keeps the files
in that account’s rworker state directory with private permissions. The
default limits are seven days from modification, 2 GiB total, and 32 MiB
per job. When space runs out it removes the oldest snapshots first.

Use `rworker logs WORKER [JOB] --cached` with the exact worker name, including
after the worker is removed. The control-host SSH connection must use the
same Unix account as the collector. Use one worker-login context for
both collection and cached reads. Set `RWORKER_LOG_CACHE_READ=1` to prefer
cached results for ordinary finite log reads and seed the TUI log preview.
Finite reads fall back to the worker on a cache miss. Following logs uses
the live worker connection.

Each `rworker exec` run registers an implicit job called `exec`. Its
combined output is available to `rworker logs -f` while the worker is live.
The caller still receives stdout, stderr and the command's exit status.
A command that finishes before discovery can be read from the archive
when log archiving is enabled.

Use `rworker logs WORKER --redact` to mask common credential formats in
the local display, including labeled passwords, authorization headers, supported
tokens, URL passwords and private key blocks. This works with archived snapshots
and with `--follow`. The archive retains the original bytes. Automatic matching
can miss unrecognized or unlabeled secrets. Add an extra pattern with
`--redact-regex REGEX`.

For workers created on a control host with log archiving enabled, `exec`
streams output as usual and sends bounded verbatim tails of stdout and
stderr to the control host before deleting the worker. Read the retained
`exec` job with `rworker logs WORKER exec --cached`. The collector keeps up to
1,024 worker history records for 30 days from last observation.

## Lifecycle history

`rworker events` lists retained worker and job transitions from the controller.
Use `rworker events -w` to follow new entries, or add `--since-now` to skip the
backlog. The TUI's `E` view follows the same feed. Use `--json` for an array
when listing and newline-delimited JSON when watching.

The controller retains up to 8192 events for the configured log retention age.
It records worker events as inventory refreshes and polls job states every
three seconds when the log collector has a configured operator. The history
starts at activation and can miss transitions between observations.

## Terminal UI

The TUI and `rworker ls -w` follow worker inventory over a WebSocket on the
controller and fall back to polling if the connection drops. By default they
use the configured SSH control connection to reach the socket. To use mutual
TLS directly, set `RWORKER_API_URL` to the control host’s HTTPS origin and
supply curl client certificate options through `RWORKER_API_CURL_CONFIG`.

Run `rworker` in a terminal, or type `rworker tui` explicitly. The TUI is an interactive front end over the same actions, in the
spirit of k9s. The worker list refreshes in the background (`-n SECONDS`,
default 5) and shows your own workers with a dot and new ones with a `+`;
a strip below it gives the selected worker's owner, profile, addresses and
working directory.

- `Alt+Shift+Q` quits from any view.
- `J` toggles Jobs and Workers. Jobs shows running and registered jobs by
  default. Press `d` in Jobs to include completed and inactive jobs.
- `h` shows or hides retired workers inline. Press Enter on a retired worker
  to view its retained logs.
- `⏎` opens the worker's jobs, `⏎` on a job follows its log; `l` follows
  all jobs of the worker, `L` all workers, `E` the lifecycle events.
- `s` logs in over ssh (the TUI comes back when the session ends), `i`
  inspects, `p` lists the job processes.
- `e`, `t`, `c` and `d` extend, retopic, claim and delete the worker (`d`
  asks first); `n` creates one, `X` prunes yours.
- In a log, scrolling up pauses it and `G` follows again; `/` filters
  lines, `w` wraps them.
- `:` runs any other action (`sync`, `cp`, `exec`, `setup`, `nix-cache`, ...) with
  its output in a view of its own; `?` lists all keys.

The TUI runs this program again for every list, stream and action, so it
behaves exactly like the commands and never waits on the network while it
draws.

## Environment

| Variable | Meaning |
| --- | --- |
| `RWORKER_HOST` | Control host (an SSH destination) |
| `RWORKER_KEY` | Public key file handed to new workers (default `~/.ssh/id_ed25519.pub`) |
| `RWORKER_USER` | Login user override |
| `RWORKER_AGENT_ID`, `CONCORD_AGENT_ID` | Agent id recorded on `up` and `claim` |
| `RWORKER_ICONS=0`, `NO_NERD_FONTS` | No Nerd Font glyphs (creating agent, worker image); `ls --no-icons` for one call |
| `RWORKER_PROBE_TIMEOUT` | Seconds to wait for each overlay address (default 4) |
| `RWORKER_AWS_CREDENTIALS_FILE` | AWS credentials file `setup nix` (and `nix-cache`) reads its profiles from (else `AWS_SHARED_CREDENTIALS_FILE`, else `~/.aws/credentials`) |
| `RWORKER_SSH_CONTROL_MASTER=0` | No shared SSH connection to the control host |
| `NO_COLOR` | No colors, also not in help |
