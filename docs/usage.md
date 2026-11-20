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

- `up` creates an ad-hoc worker and waits until it answers (usually 1-4
  minutes); `--ssh` logs in right away, `--json` prints it for scripts.
- `extend NAME --ttl 2h` moves the expiry; `down NAME` deletes it; `prune`
  deletes several (by default your own).
- A worker still in use when it expires stays until it has been idle for an
  hour, up to its maximum lifetime.
- Always pass `--topic` (what it is for) and `--agent-id` when you create
  one; `claim NAME` takes a worker over when you reuse it.

## Worker names

A name may be abbreviated: an exact name wins, then a unique prefix, then a
unique substring (`rworker ssh 3e19`). An ambiguous fragment lists the
matches. `USER@NAME` picks the login user; the default is `agent` on ad-hoc
workers this host created and yourself on all others. Workers this host has
not seen yet are looked up on the control host on first use.

## Jobs

`start NAME --name JOB -- 'COMMAND'` runs a command in the background on the
worker, `jobs NAME` lists them with their state, `logs NAME [JOB]` prints
their output and `logs -f` follows every job on every worker, with
lifecycle events and a periodic worker count on stderr.

## Terminal UI

`rworker tui` is an interactive front end over the same actions, in the
spirit of k9s. The worker list refreshes in the background (`-n SECONDS`,
default 5) and shows your own workers with a dot and new ones with a `+`;
a strip below it gives the selected worker's owner, profile, addresses and
working directory.

- `⏎` opens the worker's jobs, `⏎` on a job follows its log; `l` follows
  all jobs of the worker, `L` all workers, `E` the lifecycle events.
- `s` logs in over ssh (the TUI comes back when the session ends), `i`
  inspects, `p` lists the job processes.
- `e`, `t`, `c` and `d` extend, retopic, claim and delete the worker (`d`
  asks first); `n` creates one, `X` prunes yours.
- In a log, scrolling up pauses it and `G` follows again; `/` filters
  lines, `w` wraps them.
- `:` runs any other action (`sync`, `cp`, `exec`, `nix-cache`, ...) with
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
| `RWORKER_ICONS=0` | No Nerd Font glyphs for the creating agent |
| `RWORKER_PROBE_TIMEOUT` | Seconds to wait for each overlay address (default 4) |
| `RWORKER_AWS_CREDENTIALS_FILE` | AWS credentials file `nix-cache` reads its profiles from (else `AWS_SHARED_CREDENTIALS_FILE`, else `~/.aws/credentials`) |
| `RWORKER_SSH_CONTROL_MASTER=0` | No shared SSH connection to the control host |
| `NO_COLOR` | No colors, also not in help |
