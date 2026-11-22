# GitHub Actions workers

The workflow lives in the repository associated with the build. The rworker
workflow provides native Linux ARM64 and x86-64 profiles and installs Nix.
The agent advertises KVM after the build user successfully opens a VM
through `/dev/kvm`.

## Controller setup

Enable `services.rworker.githubActions` on the controller. Set its
`serverName` to the agent API hostname and its `authorizedKeysFile` to a
runtime file readable by the dedicated `rworker-github-relay` SSH account.
Point the hostname to the controller and configure ACME for nginx. Allow
the hosted runner to reach HTTPS and the controller SSH port.

Add `githubActions` to the controller runtime JSON:

```json
{
  "githubActions": {
    "repository": "owner/project",
    "repositoryId": "12345678",
    "tokenFile": "/run/secrets/rworker-github-actions-token",
    "stateDirectory": "/var/lib/rworker-github"
  }
}
```

The GitHub credential needs Actions write access in the configured
repository. The controller service user needs read access to the
credential file. The workflow currently dispatches from `main`.

## Runner registration

The workflow uses these repository variables and secrets.

- `RWORKER_AGENT_URL` is the controller's agent API HTTPS base URL.
- `RWORKER_GATEWAY` is the SSH gateway login, in `user@host` form.
- `RWORKER_GATEWAY_KNOWN_HOSTS` contains the pinned gateway host key.
- The `RWORKER_GATEWAY_KEY` secret holds the SSH key for a dedicated gateway
  account restricted to remote forwarding on loopback.

The controller persists the lease before dispatching the workflow. The
workflow receives the lease ID and profile. Its first request is
`POST /v1/agent/jobs/<id>/claim`, with the GitHub OIDC bearer token and the
detected profile, architecture, and capabilities. The controller verifies
the signature, issuer, audience, lifetime, repository ID, workflow, branch,
event, exact run ID, first attempt, and hosted runner environment. It
returns the lease token, expiry, and operator SSH public key.

The runner authenticates subsequent requests with the lease token. It
registers the forwarded port and runner host key at `POST /ready` and checks
that the lease is still active at `POST /heartbeat`. Both paths are relative
to `/v1/agent/jobs/<id>`.

## Interactive worker leases

`rworker up` with a GitHub profile reserves a runner until its lease expires
or `rworker down` cancels the run. During that window, `rworker ssh` opens a
normal SSH session and `sync` and `cp` use the same route. `rworker exec` owns
its lease and releases it after collecting the command result and outputs.
When the controller has log archiving configured, use `rworker logs
--cached WORKER` to retrieve the archived output after the lease ends.

The agent authenticates the assignment over HTTPS, starts a loopback-only
SSH server with a freshly generated host key, and establishes an outbound
reverse forwarding session to a dedicated gateway account. The gateway
allocates a loopback port for that lease and accepts only remote forwarding
on the runner credential. The agent reports the pinned host key and the
assigned endpoint through the authenticated lease. The client connects to that port through the authenticated controller SSH
connection. The gateway must run on the controller host and the operator
SSH account must allow local forwarding. Operator SSH stays
end-to-end encrypted to the runner. The build account accepts the requesting
operator's public key and has no sudo access.

Closing a lease revokes new gateway connections, terminates existing sessions,
and cancels the workflow. The agent also stops serving at its local deadline
if the controller is unreachable. A runner is ready only after the agent has
registered its SSH endpoint and the controller can reach it.


For an interactive ARM worker, use `rworker up --profile github-arm64 --ssh`.
For a one-shot build from the current checkout, run
`rworker exec --profile github-arm64 --sync .:src -- 'cd src && cargo test'`.
Use `github-x64` for a native x86-64 runner.
