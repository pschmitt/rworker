"""Home Assistant actions for the controller's worker operations."""

from functools import partial

from aiohttp import ClientError, ClientResponseError
from homeassistant.core import SupportsResponse
from homeassistant.exceptions import HomeAssistantError, ServiceValidationError, Unauthorized
import voluptuous as vol

DOMAIN = "rworker"
ACTIONS = {
    "create_worker": ("create", (), True),
    "delete_worker": ("delete", ("worker",), False),
    "extend_worker": ("extend", ("worker", "ttl"), False),
    "set_topic": ("topic", ("worker", "topic"), False),
    "claim_worker": ("claim", ("worker", "topic"), False),
    "start_job": ("start", ("worker", "job", "command"), False),
    "register_log": ("register", ("worker", "job", "path"), False),
    "renew_cache_token": ("renew-token", ("worker",), False),
    "exec": ("exec", ("command",), True),
}


async def _call(hass, action, call):
    if call.context.user_id is not None:
        user = await hass.auth.async_get_user(call.context.user_id)
        if user is None or not user.is_admin:
            raise Unauthorized()
    loaded = hass.config_entries.async_loaded_entries(DOMAIN)
    fields = dict(call.data)
    selected = fields.pop("config_entry_id", None)
    entries = [entry for entry in loaded if selected is None or entry.entry_id == selected]
    if len(entries) != 1:
        raise ServiceValidationError("Select exactly one connected rworker controller")
    try:
        result = await entries[0].runtime_data.client.action(action, fields)
    except ClientResponseError as err:
        raise HomeAssistantError(f"Controller rejected the action (HTTP {err.status})") from None
    except (ClientError, OSError, TimeoutError, ValueError):
        raise HomeAssistantError("Controller response unavailable. Check before retrying.") from None
    if call.return_response:
        return result
    return None


def async_register_actions(hass):
    """Register one service per controller action."""
    for service, (action, required, provision) in ACTIONS.items():
        schema = {vol.Optional("config_entry_id"): str}
        schema.update({vol.Required(key): str for key in required})
        if provision:
            schema.update({
                vol.Optional("profile", default="standard"): str,
                vol.Optional("ttl", default="2h"): str,
                vol.Optional("topic", default="Home Assistant"): str,
            })
        hass.services.async_register(
            DOMAIN,
            service,
            partial(_call, hass, action),
            schema=vol.Schema(schema),
            supports_response=SupportsResponse.OPTIONAL,
        )
