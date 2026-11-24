# GitLab CI workers

Both GitLab profiles lease a Linux x86-64 job on the project's regular
untagged runners. The 30-minute default lease includes queue time, and the
maximum is two hours.

| Profile | Worker image | Bootstrap |
| --- | --- | --- |
| `gl-x86_64` (alias `gl`) | `ubuntu:24.04` | Installs Nix, SSH and the runner tools with APT in each job |
| `gl-x86_64-lease` | `ghcr.io/pschmitt/rworker:lease` | Starts the bundled Nix daemon and SSH server with the prebuilt runtime |

```sh
rworker up --profile gl-x86_64 --topic "interactive build" --ssh
rworker exec --profile gl-x86_64-lease --sync .:src -- 'cd src && nix build'
```

`down` closes the lease and requests cancellation of its pipeline.
`exec` releases the lease after collecting the command's result. SSH, sync,
and file copies use the controller's loopback relay and the pinned runner
host key. `up` and `exec` give the worker this host's Nix cache credentials,
flake input tokens and a niks3 push token by default, as for OpenStack
workers; see [Nix cache and tokens](github-actions.md#nix-cache-and-tokens).
`--no-nix-cache`, `--no-nix-auth` and `--no-niks3-token` keep them off the runner.

To run the rworker CLI itself inside a GitLab job, use the
[OCI client image](oci-client.md).

## Project and runner

Use a private GitLab 18.4 or newer project with a protected `main` branch.
The project must contain the root `.gitlab-ci.yml` and the runner agent
script. The configuration only creates pipelines for API requests on
protected `main`.

The job uses the project's regular untagged runners. It needs a Linux x86-64
container executor with a root bootstrap user, no host Docker socket, and
private build storage. Allow
HTTPS access to GitLab, Ubuntu package mirrors, Nix caches, and the
controller, plus SSH access to the controller. The runner also needs to pull
the selected image from Docker Hub or GHCR. Commands run under an
unprivileged local account with a job-local Nix store.

## Lease storage

Both profiles default to a file-backed LUKS2 volume with a random per-lease
key. The build user's home, temporary directories, and writable Nix store
and database use this volume. The runner must permit loop devices,
device-mapper, and ext4, bind, and overlay mounts. The bootstrap needs
`CAP_SYS_ADMIN` and access to the loop and device-mapper devices inside
the job container. The Ubuntu job installs the storage tools with APT,
and the prebuilt image includes them. Lease startup fails if setup
fails. To opt out on a runner without those capabilities, set the project
CI/CD variable `RWORKER_UNENCRYPTED_PROFILES` to `gl-linux-x64`,
`gl-linux-x64-lease`, or both separated by a comma.

Encryption runs in the original job and has no KVM dependency. Nix VM
tests can still use `/dev/kvm` when the runner makes it available to the
build user. Use a trusted runner with encrypted or disabled swap if build
data in memory also needs protection. CI logs and uploaded artifacts are
outside the lease volume.

See [CI lease storage](usage.md#ci-lease-storage) for the common layout and
operator commands.

For private services reachable over Tailscale or NetBird, see
[Mesh access from CI workers](ci-mesh.md).

Configure these protected project CI/CD variables.

- `RWORKER_AGENT_URL` is the controller's HTTPS URL for agent registration.
- `RWORKER_GATEWAY` is the SSH relay login, for example
  `rworker-gitlab-relay@agent.example.com`.
- `RWORKER_GATEWAY_KNOWN_HOSTS` contains the pinned relay host key.
- `RWORKER_GATEWAY_KEY` is the private relay key as a protected variable.
  Use a separate key from the GitHub Actions relay.

## Controller

Enable `services.rworker.gitlabCi` and set its `serverName` and
`authorizedKeysFile`. Point that hostname to the controller and configure
nginx ACME. Give the operator SSH account permission to forward to the
controller's loopback relay port.

Add the GitLab project to the controller runtime JSON.

```json
{
  "gitlabCi": {
    "instance": "https://gitlab.example.com",
    "project": "owner/project",
    "projectId": 12345,
    "tokenFile": "/run/secrets/rworker-gitlab-token",
    "stateDirectory": "/var/lib/rworker-gitlab"
  }
}
```

Use an API credential with permission to create and cancel pipelines on
protected `main`. The controller and reaper need read access to the token
file. The controller stores the pipeline ID before accepting a runner
claim. It checks the GitLab ID token's issuer, audience, validity, project,
pipeline, protected ref, and CI configuration path before
issuing a lease token. The runner uses that token for SSH readiness and
heartbeats.

Set `gitlabCi.runnerId` only if you want to restrict claims to a specific
runner. In that case, ensure the project's job can be assigned to that runner.


## Compare bootstrap times

Create API pipelines on protected `main` with an empty `job_id`.
They start a local SSH server, authenticate as the build user, check the Nix
store, and exit without requesting a worker lease.

```sh
glab api 'projects/12345/pipeline' --method POST --input - <<'JSON'
{"ref":"main","inputs":{"job_id":"","profile":"gl-linux-x64"}}
JSON
glab api 'projects/12345/pipeline' --method POST --input - <<'JSON'
{"ref":"main","inputs":{"job_id":"","profile":"gl-linux-x64-lease"}}
JSON
```

The `rworker bootstrap` line measures setup and the runtime check inside
the container. Compare the GitLab job duration for the full job, including
image preparation. Queue time and image cache state depend on the runner.

In one comparison on different regular instance runners, Ubuntu took
16.0 seconds inside the container and 24.5 seconds overall. The lease
image took 0.5 seconds inside the container and 25.6 seconds overall.
The prebuilt image cut bootstrap work, while job preparation accounted
for more time in that run.

GitLab checks for an updated image on every job. The lease runtime is
also published as `ghcr.io/pschmitt/rworker-lease:latest`.
