"""Connect a Home Assistant entry to an rworker controller."""

import voluptuous as vol
from aiohttp import ClientError, ClientResponseError
from homeassistant import config_entries
from homeassistant.const import CONF_TOKEN, CONF_URL
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.selector import TextSelector, TextSelectorConfig, TextSelectorType
from yarl import URL

from .api import Controller


class RworkerConfigFlow(config_entries.ConfigFlow, domain="rworker"):
    """Validate access with the first pushed snapshot."""

    VERSION = 1

    async def async_step_user(self, user_input=None):
        """Configure the controller URL and access token."""
        errors = {}
        if user_input is not None:
            try:
                url = URL(user_input[CONF_URL].strip()).with_path("")
                supplied = URL(user_input[CONF_URL].strip())
                if (
                    supplied.scheme not in ("http", "https")
                    or not supplied.host
                    or supplied.user is not None
                    or supplied.query_string
                    or supplied.fragment
                    or supplied.path != "/"
                ):
                    raise ValueError
                token = user_input[CONF_TOKEN].strip()
                await self.async_set_unique_id(str(url))
                self._abort_if_unique_id_configured()
                await Controller(
                    async_get_clientsession(self.hass), str(url).rstrip("/"), token
                ).probe()
            except ClientResponseError as err:
                errors["base"] = (
                    "invalid_auth" if err.status in (401, 403) else "cannot_connect"
                )
            except (ClientError, OSError, TimeoutError, ValueError):
                errors["base"] = "cannot_connect"
            else:
                return self.async_create_entry(
                    title=url.host, data={CONF_URL: str(url).rstrip("/"), CONF_TOKEN: token}
                )
        return self.async_show_form(
            step_id="user",
            data_schema=vol.Schema({
                vol.Required(CONF_URL): str,
                vol.Required(CONF_TOKEN): TextSelector(
                    TextSelectorConfig(type=TextSelectorType.PASSWORD)
                ),
            }),
            errors=errors,
        )

    async def async_step_reauth(self, entry_data):
        """Open the credential replacement form."""
        return await self.async_step_reauth_confirm()

    async def async_step_reauth_confirm(self, user_input=None):
        """Validate a replacement token before reloading."""
        entry = self._get_reauth_entry()
        errors = {}
        if user_input is not None:
            token = user_input[CONF_TOKEN].strip()
            try:
                await Controller(
                    async_get_clientsession(self.hass), entry.data[CONF_URL], token
                ).probe()
            except ClientResponseError as err:
                errors["base"] = (
                    "invalid_auth" if err.status in (401, 403) else "cannot_connect"
                )
            except (ClientError, OSError, TimeoutError, ValueError):
                errors["base"] = "cannot_connect"
            else:
                return self.async_update_reload_and_abort(
                    entry, data_updates={CONF_TOKEN: token}
                )
        return self.async_show_form(
            step_id="reauth_confirm",
            data_schema=vol.Schema({
                vol.Required(CONF_TOKEN): TextSelector(
                    TextSelectorConfig(type=TextSelectorType.PASSWORD)
                )
            }),
            errors=errors,
        )
