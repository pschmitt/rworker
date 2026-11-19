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

### Fixed

- Job commands (`logs`, `jobs`, `ps`, `logs -f`) on a worker seen for the
  first time printed nothing: the control-host lookup swallowed the job
  manager script. `prune` with several workers deleted only the first.
