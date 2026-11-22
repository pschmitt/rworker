"""Worker counts and inventory diagnostics."""

from homeassistant.components.sensor import (
    SensorDeviceClass,
    SensorEntity,
    SensorStateClass,
)
from homeassistant.const import EntityCategory

from .entity import RworkerEntity


async def async_setup_entry(hass, entry, async_add_entities):
    """Add aggregate sensors for the controller."""
    async_add_entities(
        InventorySensor(entry, key)
        for key in (
            "workers",
            "ad_hoc_workers",
            "pool_workers",
            "active_workers",
            "last_snapshot",
        )
    )


class InventorySensor(RworkerEntity, SensorEntity):
    """Summarize the latest inventory snapshot."""

    def __init__(self, entry, key):
        super().__init__(entry, entry.runtime_data.coordinator, key)
        if key == "last_snapshot":
            self._attr_device_class = SensorDeviceClass.TIMESTAMP
            self._attr_entity_category = EntityCategory.DIAGNOSTIC
        else:
            self._attr_state_class = SensorStateClass.MEASUREMENT

    @property
    def native_value(self):
        """Return the count or the time of the snapshot."""
        data = self.coordinator.data
        if data is None:
            return None
        key = self.translation_key
        if key == "last_snapshot":
            return data.received
        live = [row for row in data.workers.values() if not row.get("retired")]
        if key == "ad_hoc_workers":
            return sum(row.get("kind") == "adhoc" for row in live)
        if key == "pool_workers":
            return sum(
                row.get("kind") not in (None, "", "adhoc", "unknown") for row in live
            )
        if key == "active_workers":
            return sum(row.get("status") == "ACTIVE" for row in live)
        return len(live)
