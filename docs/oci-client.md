# OCI client image

`ghcr.io/pschmitt/rworker:latest` contains the Nix-built rworker CLI,
OpenSSH, rsync, and a shell for GitLab CI jobs. GHCR serves native
Linux amd64 and arm64 images. Pin the manifest digest for repeatable jobs.

```yaml
check-workers:
  image: ghcr.io/pschmitt/rworker:latest
  script:
    - rworker profiles
```

Set `RWORKER_HOST` to the controller hostname. Supply the controller's pinned
host key and the job's SSH credentials through protected CI variables.
Install the host key in `$HOME/.ssh/known_hosts`, configure the SSH login,
and set `RWORKER_KEY` to the corresponding public key file for leases.
See [Usage](usage.md) for the client settings.

Build and load the image for your current Linux architecture with:

```sh
nix build .#client-image
docker load < result
docker run --rm ghcr.io/pschmitt/rworker:latest rworker help
```

The separate [GitLab lease image](gitlab-ci.md) bundles the worker runtime
and serves the `gl-x86_64-lease` profile.
