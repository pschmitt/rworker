# Home Assistant

Install rworker from this repository as a custom integration. Use the repository
URL `https://github.com/pschmitt/rworker` with category **Integration** in HACS,
or copy `custom_components/rworker` into your Home Assistant configuration
directory. Restart Home Assistant after installing it.

[![Open in Home Assistant Marketplace](https://my.home-assistant.io/badges/marketplace_repository.svg)](https://my.home-assistant.io/redirect/marketplace_repository/?owner=pschmitt&repository=rworker&category=integration)

Run a controller from a revision that includes the automation API. Enable its
web listener and configure `RWORKER_WEB_TOKEN_FILE` as for the web console.
Expose `/v1/automation/workers` with WebSocket upgrade support and
`/v1/automation/action` through the same trusted HTTPS endpoint. Forward the
Authorization header. If browser login is enforced at the proxy, exempt those
two paths from the browser login redirect so the controller can validate the
bearer token directly. Set the proxy request timeout to at least the duration of
the longest action you intend to run.

In Home Assistant, add **rworker** under **Settings → Devices & services**.
Enter the controller base URL and its access token. The token grants worker
management and command execution on workers. Store it like an admin credential.

## Entities

Each controller has sensors for total live workers, ad hoc workers, pool workers,
and workers in the ACTIVE state. The **Worker lifecycle** event entity reports
worker additions, removals, and status changes. Its attributes include the
worker name and, for status transitions, the old and new status.

The integration receives inventory snapshots over WebSocket without recurring
HTTP requests. Connectivity and last snapshot time are diagnostic entities.
After reconnecting, the first snapshot establishes a new baseline. Transitions
that occurred during the outage are not replayed. Downloadable diagnostics
contain aggregate health and counts.

## Actions

Use `rworker.create_worker`, `rworker.delete_worker`, `rworker.extend_worker`,
`rworker.set_topic`, `rworker.claim_worker`, `rworker.start_job`,
`rworker.register_log`, `rworker.renew_cache_token`, or `rworker.exec`.
For multiple controllers, set `config_entry_id` to select one. Direct action
calls require an administrator. Automations can call the actions.

`start_job` starts a named job on an existing worker. `exec` creates a temporary
worker, runs a command, and follows the CLI cleanup behavior. It can run for up
to an hour. Use `response_variable` to capture the controller's output:

```yaml
action: rworker.exec
data:
  command: uname -m
  profile: standard
  ttl: 2h
  topic: Home Assistant check
response_variable: result
```

Treat action output as potentially sensitive. If an action times out or loses
its connection, check the worker inventory or job before retrying.
