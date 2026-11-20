# Control host parity contract

Replace the Python control host with subcommands of the existing Rust
binary. The process boundary is one invocation of
`rworker server CONFIG ACTION [ARGS...]` with the same settings and
arguments as the Python entry point. The SSH entry point on the control
host continues to accept `rworker ACTION [ARGS...]`. The preconfigured
local client reaches the same handlers through a Unix socket.

## Observable behavior

Compare exit status, stdout (including JSON keys, types, values, and array
order), ordered `rworker-progress STAGE` stderr events, user-facing
errors, and provider, filesystem, and worker SSH side effects. Compare
JSON structurally. Normalize only generated names, credentials, timestamps,
and signed token bytes by checking their format and claims before
substituting deterministic values. Runtime traceback formatting and
diagnostic timestamps may differ. Keep `nixbot-worker` metadata and the
`nixbot-worker.metadata` boot parameter intact.

| Commands | Cases to characterize |
| --- | --- |
| profiles, ls | Defaults and overrides, empty inventory, ready and booting workers, missing routes, owner and creator metadata |
| up | TTL syntax and limits, profiles and overrides, SSH keys, progress, peer routing, token install, failure cleanup |
| down, extend, topic, claim | Target selection, ownership and force, missing and ambiguous names, metadata updates, TTL caps, topic clearing, token renewal |
| niks3-token | Owner check, eligible and ineligible worker, claims and expiry, provider and signing failures |
| reap | Boot timeout, unexpired, expired idle, expired busy, unknown activity, last-active grace, token renewal, maximum lifetime |
| autoscale | Pool bounds, demand and idle transitions, waiting for readiness, stale or failed workers, provider failures |

Use the Python implementation as the oracle. Run both entry points against
fresh copies of identical fake OpenStack, NetBird, Tailscale, niks3 and SSH
state. Capture ordered mutations and resulting state as well as the CLI
response. Supply a deterministic clock or use equivalent relative times on
both sides. Cover invalid syntax and failures before and after the first
mutation. Keep real credentials and routes outside the harness. Report the
number of compared cases and differences.

## Cutover safety

Characterize destructive decisions with fakes, including the existing
maximum-lifetime branch, which runs before the activity probe. Protect
in-use workers during the real cutover. Treat any required change to
deletion policy as a separate behavior change and test it separately.

Verify the NixOS module uses runtime secret-file paths and installs the
local client. Keep the SSH command path and existing browser client
working. Verify the autoscaler on its host and the reaper on the control
host. Capture inventory before and after each switch and check owner,
topic, SSH sessions and running jobs. Coordinate the timer handoff so
only one reaper can run.
