# Mesh access from CI workers

GitHub Actions and GitLab CI leases can join Tailscale, NetBird, or both.
Mesh access is off unless enabled for the profile in the controller’s
runtime JSON. Both clients run in userspace and provide loopback SOCKS
proxies. The controller’s existing SSH relay handles operator connections.

## Controller configuration

Add a `meshProfiles` object to `githubActions` or `gitlabCi`. Use the
profile ID sent to the forge. For example, this GitHub settings fragment
enables both providers on x86-64.

```json
{
  "githubActions": {
    "meshProfiles": {
      "linux-x64": {
        "tailscale": true,
        "netbird": {
          "group": "ci-workers",
          "managementUrl": "https://netbird.example.com"
        }
      }
    }
  }
}
```

The GitHub profile IDs are `linux-x64` and `linux-arm64`. The GitLab
profile IDs are `gl-linux-x64` and `gl-linux-x64-lease`. Omit either
provider to leave it disabled. Preserve the rest of the forge settings
when adding this fragment.

The controller uses the existing top-level Tailscale OAuth configuration
and tag, and the existing `netbirdApi` and `netbirdTokenFile` settings.
The NetBird group must already exist. The Tailscale tag and NetBird group
determine what a worker can reach. Give the controller account read access
to the provider credential files. The CI project needs no mesh API secrets.

The controller issues short-lived, single-use registration keys after an
authenticated job claims the lease. The runner loads the requested clients
from the repository’s locked nixpkgs revision, joins with an ephemeral
peer identity, and starts SSH after the configured proxies are ready.
If the forge uses a separate repository, keep its runner script and
`flake.lock` in sync with the controller version.
Client state is kept in memory or in a private directory on `/dev/shm`.
When the lease ends, the agent stops the clients and the providers reap
the offline ephemeral peers.

## Use the proxies

Each configured provider exposes an SSH session variable with its SOCKS
endpoint. Pass the desired proxy to an application explicitly.

```sh
curl --proxy "$RWORKER_TAILSCALE_PROXY" https://service.example.com/
curl --proxy "$RWORKER_NETBIRD_PROXY" https://service.example.com/
```

The proxies serve clients in the worker job only. Applications need SOCKS
support. A system service such as the Nix daemon has its own environment
and needs separate proxy configuration to use private endpoints.
