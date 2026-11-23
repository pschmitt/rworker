# Mesh access from CI workers

GitHub Actions and GitLab CI leases can join Tailscale, NetBird, or both.
Mesh access is off unless enabled for the profile in the controller’s
runtime JSON. Both clients run in userspace and provide loopback SOCKS
proxies. The controller’s existing SSH relay handles operator connections.
Both mesh clients block inbound mesh connections.

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

### Enable a provider after startup

Configure `meshOnDemand` under `githubActions` or `gitlabCi` with the same
profile and provider structure as `meshProfiles`. Entries under
`meshOnDemand` allow manual activation without starting the provider at
lease creation. Providers listed under `meshProfiles` can also be
re-enrolled during an active lease. For example, this allows Tailscale
activation on GitHub x86-64.

```json
{
  "githubActions": {
    "meshOnDemand": {
      "linux-x64": { "tailscale": true }
    }
  }
}
```

```sh
proxy=$(rworker setup tailscale WORKER)
rworker ssh WORKER "curl --proxy '$proxy' https://service.example.com/"
rworker setup netbird WORKER --json
```

The lease owner can run either command. A repeat replaces that provider's
peer on the same SOCKS address, without restarting the worker or SSH.
The returned address is loopback on the worker. For providers first
started after SSH setup, use the returned URL explicitly.

On-demand setup needs a current GitHub runner agent or the GitLab
`gl-x86_64-lease` profile. The GitLab Ubuntu profile's Nix store is
writable by the build user, so it runs its mesh startup only before
the build user gets access.

### Use a provider started at lease creation

Each configured provider exposes an SSH session variable with its SOCKS
endpoint. Pass the desired proxy to an application explicitly.

```sh
curl --proxy "$RWORKER_TAILSCALE_PROXY" https://service.example.com/
curl --proxy "$RWORKER_NETBIRD_PROXY" https://service.example.com/
```

The proxies serve clients in the worker job only. Applications need SOCKS
support. A system service such as the Nix daemon has its own environment
and needs separate proxy configuration to use private endpoints.
