"""Worker inventory lifecycle events."""

from homeassistant.components.event import EventEntity
from homeassistant.core import callback

from .entity import RworkerEntity


async def async_setup_entry(hass, entry, async_add_entities):
    """Add one event entity for the controller."""
    async_add_entities([WorkerEvents(entry)])


class WorkerEvents(RworkerEntity, EventEntity):
    """Emit additions, removals and status transitions."""

    def __init__(self, entry):
        super().__init__(entry, entry.runtime_data.coordinator, "worker_events")
        self._attr_event_types = [
            "worker_added",
            "worker_removed",
            "worker_status_changed",
        ]

    @callback
    def _handle_coordinator_update(self):
        if self.coordinator.last_update_success and self.coordinator.data.events:
            for event in self.coordinator.data.events:
                attributes = {
                    key: value for key, value in event.items() if key != "event_type"
                }
                self._trigger_event(event["event_type"], attributes)
                self.async_write_ha_state()
        else:
            self.async_write_ha_state()
