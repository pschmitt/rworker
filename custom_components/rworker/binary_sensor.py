"""WebSocket connectivity diagnostic."""

from homeassistant.components.binary_sensor import BinarySensorDeviceClass, BinarySensorEntity
from homeassistant.const import EntityCategory

from .entity import RworkerEntity


async def async_setup_entry(hass, entry, async_add_entities):
    """Add a connectivity indicator."""
    async_add_entities([ConnectionSensor(entry)])


class ConnectionSensor(RworkerEntity, BinarySensorEntity):
    """Show whether snapshots can be received."""

    _attr_device_class = BinarySensorDeviceClass.CONNECTIVITY
    _attr_entity_category = EntityCategory.DIAGNOSTIC

    def __init__(self, entry):
        super().__init__(entry, entry.runtime_data.connected, "connection")

    @property
    def is_on(self):
        """Return whether the inventory stream is live."""
        return self.coordinator.data
