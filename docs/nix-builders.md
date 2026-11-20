# On-demand Nix builders

The rworker gateway gives Nix a stable `ssh-ng` endpoint. When Nix opens its
first connection, the gateway provisions a worker using the chosen controller
profile and opens a pinned SSH connection to its Nix daemon. Further
connections share that worker. Regular local builds and binary substitutes
continue to work through Nix's normal scheduler.

## Gateway host

Import `inputs.rworker.nixosModules.default` on the control host. Enable the
gateway beside an existing `services.rworker` configuration.

```nix
{
  services.rworker = {
    enable = true;
    settingsFile = "/run/secrets/rworker-settings.json";
    reaper.enable = true;
    builderGateway = {
      enable = true;
      authorizedKeysFile = "/run/secrets/rworker-builder-authorized-keys";
      profile = "standard";
      # Optional. A JSON object mapping SSH source addresses to machine names.
      clientNamesFile = "/run/secrets/rworker-builder-client-names.json";
      idleSeconds = 300;
      leaseSeconds = 7200;
      maxConnections = 8;
    };
  };
}
```

Put the client daemon's SSH public key in the gateway's `authorizedKeysFile`.
It must be readable by the `rworker-builder` login account. If your SSH server
has an account allowlist, add `rworker-builder` to it. The account accepts the
Nix daemon command and does not provide a shell, PTY, or forwarding.

The controller uses its existing `sshUser` and `sshKeyFile` for the second hop.
It also derives the agent login public key for provisioning from that identity.
The private key must be usable without an interactive prompt. The selected
worker image must authorize that key for a Nix build account.
The managed worker image provides `nix-remote-builder` for this purpose. Keep
client keys within one trust boundary. Clients using this gateway share a
worker store and can have the privileges of a trusted Nix user on that worker.

## Try one build

Obtain the gateway's public SSH host key from a trusted source and encode the
whole public key line with `base64 -w0`. Use a dedicated client SSH key pair
and put its private key in a runtime file readable by the local Nix daemon. The daemon normally
runs as root. The public host key identifies the gateway itself.

Use an explicit builder for the first build. Substitute the real host, key path,
pinned host key, and worker system in this example.

```sh
builder="ssh-ng://rworker-builder@builder.example.com x86_64-linux /run/secrets/rworker-builder-key 1 1 - - $gateway_public_host_key"
nix build -L --max-jobs 0 --builders "$builder" --no-link --print-out-paths \
  --impure --expr '
    let pkgs = import (builtins.getFlake "nixpkgs").outPath { };
    in pkgs.runCommand "rworker-gateway-probe-${toString builtins.currentTime}" { }
      "echo gateway-probe > $out"
  '
```

Nix reports the gateway address when it dispatches the build. Check
`rworker machines --fresh` for the leased worker, then try another build within
the idle window to check worker reuse. A cold build waits for OpenStack
provisioning and worker boot before the Nix daemon handshake can finish.

## Add the builder with NixOS

Import `inputs.rworker.nixosModules.default` on the client and set the client
options. The module enables distributed builds and appends the machine entry.

```nix
{
  services.rworker.builderClient = {
    enable = true;
    hostName = "builder.example.com";
    identityFile = "/run/secrets/rworker-builder-key";
    publicHostKey = "<base64-encoded-gateway-public-host-key>";
    systems = [ "x86_64-linux" ];
    maxJobs = 1;
    speedFactor = 1;
  };
}
```

Set `systems` and `supportedFeatures` to capabilities the leased worker actually
provides. `mandatoryFeatures` can restrict this entry to derivations asking for
particular features. The optional `nix.settings.builders-use-substitutes` setting
allows the worker to fetch dependencies from its own substituters.

## Lease lifetime

The gateway keeps one worker per controller. It serializes allocation and
shares a ready worker across up to `maxConnections` live protocol sessions.
The idle timer checks once a minute and deletes the worker after
`idleSeconds` without a session. The normal controller lifetime and reaper
remain the upper bound, including after a host crash. Configure
`leaseSeconds` with your longest expected build in mind.

Wait for the idle timer before shutting down the gateway if you want immediate
cleanup on disable. The normal controller reaper can also remove an expired
lease. Gateway activity is logged by the `rworker-builder@…` and
`rworker-builder-reap.service` units.


## Ownership, history and logs

The first authenticated SSH connection that triggers a lease determines its
owner. The gateway uses that connection's source IP, or the corresponding
name in `clientNamesFile` if one is configured. For example, a file containing
`{"192.0.2.10":"workstation"}` labels that client as `workstation`.
A warm build from another machine leaves the original owner's name in place.
The agent is displayed as `nix build`.

Use `rworker ls --all` (also `-A` or `--history`) or press `h` in the TUI
to include retired workers. Use `rworker logs WORKER` to read saved
build output. Pass a derivation log name printed in that output to read just
that log. The gateway collects completed logs while the worker is idle and
before deleting the lease. Each snapshot reads up to 32 recent derivation logs,
keeping at most the last MiB of each, further bounded by the controller's
`services.rworker.logCache` age and size settings.
