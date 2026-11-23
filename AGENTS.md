# Agent instructions

rworker is the client for ad-hoc build workers. Treat hostnames, keys and
cache credentials as sensitive; use role names and placeholders
(`control.example.com`) in shared examples.

## Working and validation

- Keep each user-visible change in its own commit and document it in
  `CHANGELOG.md`. Keep behavior changes separate from refactors.
- A new action or flag goes into `src/text/spec.txt`, the dispatch and its
  help text (`src/text/help-*.txt`); `cargo test` checks that they agree.
- `src/jobs.sh` runs on the workers and stays a shell script.
- Changes under `custom_components/rworker/` (code, translations, brand
  images) reach Home Assistant only when its copy is updated: a HACS update,
  or, where the Home Assistant configuration pins this repository as a git
  submodule, a submodule bump there. Both need a Home Assistant restart. A
  controller deploy alone does not update the integration.
- Validate with `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`,
  `cargo test`, `cargo deny check` and `nix flake check` (on a build host,
  not locally). Never create real workers as a test. Use fake providers.
- Before changing GitHub Actions, check whether
  [pschmitt/ci-workflows](https://github.com/pschmitt/ci-workflows) provides a
  reusable workflow or action, and use `@main` for its references.

- Keep profile reads, including `profiles -o wide`, off the network. Build
  rows from configuration and local snapshots. Refresh provider data in the
  background and cache successful rows when individual lookups fail.
