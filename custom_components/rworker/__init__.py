"""Push inventory and worker actions for Home Assistant."""

import asyncio
from dataclasses import dataclass
from datetime import datetime
import logging

from aiohttp import ClientError, ClientResponseError
from homeassistant.const import CONF_TOKEN, CONF_URL, Platform
from homeassistant.exceptions import ConfigEntryAuthFailed, ConfigEntryNotReady
from homeassistant.helpers.aiohttp_client import async_get_clientsession
from homeassistant.helpers.update_coordinator import DataUpdateCoordinator, UpdateFailed
from homeassistant.util import dt as dt_util

from .api import Controller
from .services import async_register_actions

DOMAIN = "rworker"
PLATFORMS = [Platform.SENSOR, Platform.BINARY_SENSOR, Platform.EVENT]
LOGGER = logging.getLogger(__name__)


@dataclass(frozen=True, slots=True)
class Inventory:
    """Latest valid snapshot and the changes since the previous snapshot."""

    workers: dict
    received: datetime
    events: tuple[dict, ...] = ()


class Runtime:
    """Own the inventory connection for one entry."""

    def __init__(self, hass, entry):
        self.entry = entry
        self.hass = hass
        self.client = Controller(
            async_get_clientsession(hass), entry.data[CONF_URL], entry.data[CONF_TOKEN]
        )
        self.coordinator = DataUpdateCoordinator(
            hass, LOGGER, config_entry=entry, name="rworker inventory"
        )
        self.connected = DataUpdateCoordinator(
            hass, LOGGER, config_entry=entry, name="rworker connection",
            always_update=False,
        )
        self.task = None

    async def listen(self):
        """Reconnect with backoff and use the next snapshot as a new baseline."""
        delay = 1
        while True:
            previous = None
            try:
                async with self.client.snapshots_context() as snapshots:
                    async for workers in snapshots:
                        events = []
                        if previous is not None:
                            for name in sorted(previous.keys() | workers.keys()):
                                old = previous.get(name)
                                new = workers.get(name)
                                was_live = old is not None and not old.get("retired")
                                is_live = new is not None and not new.get("retired")
                                if is_live and not was_live:
                                    events.append({"event_type": "worker_added", "worker": name})
                                elif was_live and not is_live:
                                    events.append({"event_type": "worker_removed", "worker": name})
                                elif is_live and old.get("status") != new.get("status"):
                                    events.append({
                                        "event_type": "worker_status_changed",
                                        "worker": name,
                                        "previous_status": old.get("status"),
                                        "status": new.get("status"),
                                    })
                        previous = workers
                        self.coordinator.async_set_updated_data(
                            Inventory(workers, dt_util.utcnow(), tuple(events))
                        )
                        self.connected.async_set_updated_data(True)
                        delay = 1
            except (ClientError, OSError, TimeoutError, ValueError) as err:
                self.connected.async_set_updated_data(False)
                self.coordinator.async_set_update_error(UpdateFailed("Inventory stream disconnected"))
                if isinstance(err, ClientResponseError) and err.status in (401, 403):
                    self.entry.async_start_reauth(self.hass)
                    return
            await asyncio.sleep(delay)
            delay = min(delay * 2, 60)


async def async_setup(hass, config):
    """Register actions for automations and the developer tools."""
    async_register_actions(hass)
    return True


async def async_setup_entry(hass, entry):
    """Set up one controller."""
    runtime = Runtime(hass, entry)
    try:
        workers = await runtime.client.probe()
    except ClientResponseError as err:
        if err.status in (401, 403):
            raise ConfigEntryAuthFailed("Controller token rejected") from None
        raise ConfigEntryNotReady("Controller unavailable") from None
    except (ClientError, OSError, TimeoutError, ValueError):
        raise ConfigEntryNotReady("Controller unavailable") from None

    entry.runtime_data = runtime
    runtime.coordinator.async_set_updated_data(Inventory(workers, dt_util.utcnow()))
    runtime.connected.async_set_updated_data(True)
    await hass.config_entries.async_forward_entry_setups(entry, PLATFORMS)
    runtime.task = entry.async_create_background_task(
        hass, runtime.listen(), "rworker inventory WebSocket"
    )
    return True


async def async_unload_entry(hass, entry):
    """Unload entities and stop reconnecting."""
    if not await hass.config_entries.async_unload_platforms(entry, PLATFORMS):
        return False
    task = entry.runtime_data.task
    task.cancel()
    await asyncio.gather(task, return_exceptions=True)
    return True
