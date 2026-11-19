# Changelog

## 0.1.0 (unreleased)

Rewritten in Rust from the bash client of nixos-config (`pkgs/rworker`).
Actions, flags, output, JSON and exit statuses are the same; see
[PARITY.md](PARITY.md) for the contract and the accepted differences.

### Fixed

- Job commands (`logs`, `jobs`, `ps`, `logs -f`) on a worker seen for the
  first time printed nothing: the control-host lookup swallowed the job
  manager script. `prune` with several workers deleted only the first.
