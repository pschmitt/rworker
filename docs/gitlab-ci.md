# GitLab CI workers

The `gl-x86_64` profile leases a Linux x86-64 GitLab CI job.
`gl` is an alias. The 30-minute default lease includes queue time, and the
maximum is two hours.

```sh
rworker up --profile gl-x86_64 --topic "interactive build" --ssh
rworker exec --profile gl-x86_64 --sync .:src -- 'cd src && nix build'
```

`down` closes the lease and requests cancellation of its pipeline.
`exec` releases the lease after collecting the command's result. SSH, sync,
and file copies use the controller's loopback relay and the pinned runner
host key. CI workers do not receive the client's private Nix cache credentials
or flake input tokens automatically.

To run the rworker CLI itself inside a GitLab job, use the
[OCI client image](oci-client.md).

## Project and runner

Use a private GitLab 18.4 or newer project with a protected `main` branch.
The project must contain the root `.gitlab-ci.yml` and the runner agent
script. The configuration only creates pipelines for API requests on
protected `main`.

The job uses the project's regular untagged runners. It needs a Linux x86-64
container executor that starts a fresh, unprivileged Ubuntu container with a
root bootstrap user, no host Docker socket, and private build storage. Allow
HTTPS access to GitLab, Ubuntu package mirrors, Nix caches, and the
controller, plus SSH access to the controller. The job installs its
dependencies and runs builds under an unprivileged local account with its
own Nix store.

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
