# GitHub Actions workers

Lease storage is encrypted by default with a file-backed LUKS2 volume and a
random key generated for each lease. The build user's home, temporary
directories, and writable Nix store and database use the volume. Setup
must succeed before the worker accepts SSH. To opt out, set the repository
Actions variable `RWORKER_UNENCRYPTED_PROFILES` to `linux-x64`,
`linux-arm64`, or both separated by a comma.

Encryption uses Linux device-mapper and leaves KVM access available to
builds wherever the runner exposes it. Workflow logs, uploaded artifacts,
and runner swap are outside the encrypted filesystem.

The hosted Ubuntu job uses passwordless `sudo` to set up the loop device,
LUKS2 mapping and mounts. The workflow installs the storage tools if
`cryptsetup` is missing. The agent restarts the Nix daemon socket after
mounting the writable store so daemon builds use the encrypted filesystem.
See [CI lease storage](usage.md#ci-lease-storage) for the layout and
operator commands.

For private services reachable over Tailscale or NetBird, see
[Mesh access from CI workers](ci-mesh.md).

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

The controller service user needs read access to the credential file. The
workflow currently dispatches from `main`. See [Authentication](#authentication)
for the credential.

## Authentication

The controller dispatches the workflow and cancels runs through the GitHub
API. Its credential needs only Actions write access (and Metadata read) on
the workflow repository, and the controller re-reads `tokenFile` on every
request, so the file may change while the controller runs.

Recommended: a dedicated GitHub App.

1. Create an App with the repository permissions Actions: read and write and
   Metadata: read, no webhook and no events, and install it only on the
   workflow repository. Generate a private key.
2. Put the numeric App ID and the PEM key into runtime secret files that
   root can read (never the Nix store).
3. Configure the module:

   ```nix
   services.rworker.githubActions = {
     enable = true;
     # serverName, authorizedKeysFile as above
     app = {
       idFile = "/run/secrets/rworker-github-app-id";
       privateKeyFile = "/run/secrets/rworker-github-app-key";
       repository = "owner/project";
     };
   };
   ```

4. Use the minted file in the runtime JSON, with the same repository:
   `"tokenFile": "/run/rworker-github-token/token"` (the module exposes it
   as `services.rworker.githubActions.app.tokenFile`). The runtime JSON is
   a file you provide, so the module cannot set it for you.

The module installs `rworker-github-token.service` and a timer. The service
signs a short-lived App JWT with the private key (the key stays on the
controller), looks up the App's installation on the repository and asks
GitHub for an installation token limited to that repository and to
`actions: write`. It writes the token atomically to
`/run/rworker-github-token/token` (group `rworker`, mode 0440). Installation
tokens last one hour, so the timer refreshes them every 20 minutes
(`app.refreshInterval`, first run 30 s after boot). A failed run is retried
every 30 s without limit: a failed oneshot unit would otherwise never be
started again by the timer. The GitHub API and reaper units want the token
unit but do not wait for it; a request before the first token fails and the
next one succeeds.

Alternative: a static fine-grained personal access token with Actions
write access on that one repository, stored in a runtime file that
`tokenFile` names. Prefer the App: its tokens are short-lived and scoped by
the installation.

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


For an interactive ARM worker, use `rworker up --profile gh-aarch64 --ssh`.
For a one-shot build from the current checkout, run
`rworker exec --profile gh-aarch64 --sync .:src -- 'cd src && cargo test'`.
Use `gh-x86_64` for a native x86-64 runner.

Both profiles carry the `github-actions` tag. `gh` is shorthand for
`gh-x86_64`. Existing scripts may still use `github-arm64` or `github-x64`.

The lease deadline includes queue time. A queued run can wait for capacity
on its selected architecture. The CLI reports when GitHub starts the run,
waits for SSH readiness, and requests cancellation if the lease expires.

Disk in `rworker profiles` shows total filesystem size in GiB for the most
recent worker on each GitHub profile. The field is blank until the first
measurement.

The runner's sshd allows SSH agent forwarding (`ssh -A`, also through the
controller relay), so a build can fetch private `git+ssh` inputs with the
client's key without copying it to the runner. TCP, Unix socket and X11
forwarding and tunnels are off.

## Nix cache and tokens

`rworker up`, `exec` and `ssh --create` with a GitHub profile (and with a
[GitLab CI](gitlab-ci.md) profile, see below) set the lease up like an
OpenStack worker, as far as the client host has something to give:

- the S3 Nix cache: the `s3://` substituters, the trusted public keys and the
  AWS profile they name;
- the client's Nix `access-tokens` (private flake inputs);
- a niks3 push token, valid until the lease ends.

`--no-nix-cache`, `--no-nix-auth` and `--no-niks3-token` skip one part,
`--no-credentials` all three (MCP: `nix_cache`, `nix_auth`, `niks3_token` and
`credentials` set to false). A failure warns and leaves a usable lease.
`rworker setup nix [cache|auth] NAME` and `rworker setup niks3-token NAME`
repeat it. `extend` does not apply: a lease has a fixed expiry, and the niks3
token expires with it.

**These credentials end up on a GitHub-hosted runner.** The repository is
public, so the workflow never prints them: the client sends them over SSH,
as stdin only, to the build user's home (`~/.config/rworker-nix`,
`~/.config/rworker-niks3`; 0700 directories, 0600 files). Use read-only
credentials, and `--no-credentials` for work that should not hold them.

How the runner picks them up (the agent script and the workflow are read from
`main`, so this applies after a push):

- The runner's SSH sessions set `NIX_USER_CONF_FILES` and
  `AWS_SHARED_CREDENTIALS_FILE` to those files and put `~/.local/bin` first
  in `PATH`, so plain `rworker ssh NAME 'nix build ...'` and `rworker start`
  jobs need no exports.
- The pushed `nix.conf` uses `access-tokens`, not `extra-access-tokens`: the
  runner's own `/etc/nix/nix.conf` has an `access-tokens` entry for its own
  GitHub token, which Nix would otherwise keep, and private inputs would 404.
- With the cache, the file also selects the lease's own store
  (`~/.local/share/nix/rworker`, the store the `rworker-nix-daemon` builder
  uses, with `build-users-group` empty). The system daemon would ignore the
  substituters of an untrusted user; the lease store is the user's own.
- `rworker-nix-daemon`, which serves the lease as a remote builder, reads the
  same files, so `builders-use-substitutes` substitutes from the cache.
- `~/.local/bin/niks3` runs the niks3 client from nixpkgs with the lease's
  token and server: `nix path-info -r PATHS | niks3 push --stdin`.

On GitLab CI the container has no system `nix.conf` tokens to override. The
`gl-x86_64` job installs Ubuntu's Nix (2.18), which cannot open `s3://`
stores: `setup nix` notices when the runner cannot read the bucket, warns,
leaves the cache out and still sets up the tokens. Use `gl-x86_64-lease` for
the cache. Its image runs a daemon for the build user, and a daemon ignores
the substituters of an untrusted user, so the agent marks the build user
trusted (`trusted-users`) and gives the daemon the credentials file. The
container is the build user's alone.
