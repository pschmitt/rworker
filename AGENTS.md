# Agent instructions

rworker is the client for ad-hoc build workers. Treat hostnames, keys and
cache credentials as sensitive; use role names and placeholders
(`control.example.com`) in shared examples.

## Working and validation

- Read [PARITY.md](PARITY.md) before changing behavior: user-visible
  changes are allowed, but each one goes in its own commit and in
  `CHANGELOG.md`, never mixed into a refactor.
- A new action or flag goes into `src/text/spec.txt`, the dispatch and its
  help text (`src/text/help-*.txt`); `cargo test` checks that they agree.
- `src/jobs.sh` runs on the workers and stays a shell script.
- Validate with `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, `cargo deny check` and `nix flake check` (on a build host,
  not locally). Never create real workers as a test; the parity harness
  fakes the control host.
- Before changing GitHub Actions, check whether
  [pschmitt/ci-workflows](https://github.com/pschmitt/ci-workflows) provides a
  reusable workflow or action, and use `@main` for its references.
