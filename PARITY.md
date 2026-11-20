# Parity contract: bash client to Rust

The Rust `rworker` replaces the bash client `pkgs/rworker/rworker.sh` of
nixos-config (copied as the test-only reference to
[`tests/parity/reference/rworker.sh`](tests/parity/reference/rworker.sh)).

## End state

A. Replacement. One standalone binary; no bindings. The seam is the command
line: everything that calls `rworker` today (people, the zsh and bash
completions, the MCP server, `rworker-nix`, the skills) keeps calling the
same command with the same arguments. Once parity is shown, nixos-config
consumes this repository as a flake input and the bash client is deleted.

## Unit of parity

One invocation: argv, environment and stdin in; stdout, stderr, exit status
and the side effects out. The side effects are

- the commands run on the control host (`ssh CONTROL rworker ARGS...`), as
  the list of words the remote shell receives,
- the commands run on workers (ssh, rsync), including the job manager
  payload on stdin,
- the files under `$XDG_STATE_HOME/rworker` (state files, `known_hosts`,
  the `.ls.json` cache).

## Must match

- Every action, flag, hidden action (`__spec`, `__workers`, `__jobs`) and
  their argument handling, including option order quirks, abbreviated
  worker names and `--json` anywhere.
- Exit statuses, including 2 for usage errors and the silent 1 where the
  bash client stopped under `set -e` (a trailing `--topic` without value).
- JSON output: the same keys, order and values; compact where the bash
  client printed `jq -c`, indented where it printed `jq .`; the control
  host's answer passed through unchanged where bash did that.
- Human output text, column alignment and colors (ANSI sequences), help
  texts and their coloring, the `ls` table and `--watch` frame.
- The job manager that runs on workers: byte-identical
  ([`src/jobs.sh`](src/jobs.sh)); it stays a shell script because it runs
  on the worker.
- Signal handling: `exec` deletes its worker on Ctrl-C/SIGTERM (exit 130),
  `ls --watch` restores the terminal and exits 0, `logs -f` stops its
  followers (exit 130/143).

## Acceptable differences

- Grouped short options and attached values are recognized before dispatch.
  Option values and arguments passed to remote commands are kept literal.
  Help flags following another option’s value are recognized.

- Regex dialect of `--topic/--agent/--agent-id`: Rust's `regex` instead of
  Oniguruma (jq). Lookaround and backreferences are rejected as invalid.
- Error wording of failures that came from jq, column or coreutils
  internals (malformed JSON from the control host, for example). The exit
  status stays.
- Shell quoting of the remote command line: different quoting, same words.
- Iteration order of what bash kept in associative arrays (`ls --watch`
  rows of gone workers, `events` deletions): sorted by name now, hash order
  before.
- Terminal setup of `ls --watch` (alternate screen, hidden cursor) is
  written as fixed xterm sequences instead of looked up with `tput`.
- `RWORKER_SSH_CONTROL_MASTER=0` replaces the build-time string rewrite of
  the Nix package (`sshControlMaster = false`, TermuxPP).
- No runtime dependency on jq, column, coreutils, gawk, sed, procps or
  util-linux; `ssh`, `ssh-keygen`, `rsync` and `nix` remain external.

## Deliberately not ported

- The bash implementation details that only existed for bash: namerefs,
  `mapfile` plumbing, the background subshells (now child processes or
  threads), `check-spec.sh` (now a unit test over the same three lists).

## Known bugs kept for parity (fix after cut-over)

1. `ssh_opts` failure is ignored by its callers (`mapfile < <(...)`): ssh
   would then run without the pinned host key options. Unreachable in
   practice (a state file always has a host key).
2. The control-host `ssh` has no `-n`, so it reads whatever stdin the
   surrounding shell code has: `prune` reads its list from a here-string
   (only the first worker gets deleted), and the job commands (`logs`,
   `jobs`, `ps`, `logs -f`) feed the job manager script on stdin, which a
   lookup on the control host (a worker seen for the first time, or a
   state file without `ips`) swallows; the worker then runs nothing and
   prints nothing. The port hands each ssh only its own input, so it does
   not reproduce this; the harness runs once with a realistic ssh (these
   cases are listed as accepted) and once with an ssh that leaves stdin
   alone (`FAKE_SSH_STDIN=0`, every case must match).
3. `forget` looks a deleted worker up on the control host again when it
   has no state file (to find its IP).
4. Only IP entries are removed from `known_hosts`; the `NAME KEY`
   (HostKeyAlias) lines stay.

## How parity is measured

[`tests/parity/run.py`](tests/parity/run.py) runs the reference script and
the Rust binary over the same corpus of invocations against the same fake
control host and fake workers (`tests/parity/fake/`), once with pipes and
once on a pseudo-terminal (colors), and compares stdout, stderr, exit
status, the recorded remote commands and the resulting state directory.
It prints the number of invocations compared and the number of
differences; accepted differences are listed by case name with the reason.
