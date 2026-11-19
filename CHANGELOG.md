# Changelog

## 0.1.0 (unreleased)

Rewritten in Rust from the bash client of nixos-config (`pkgs/rworker`).
Actions, flags, output, JSON and exit statuses are the same; see
[PARITY.md](PARITY.md) for the contract and the accepted differences.

### New

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
