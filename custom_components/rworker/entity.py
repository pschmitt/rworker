"""Shared controller device and entity identity."""

from homeassistant.helpers.device_registry import DeviceInfo
from homeassistant.helpers.update_coordinator import CoordinatorEntity


class RworkerEntity(CoordinatorEntity):
    """An entity attached to the configured controller."""

    _attr_has_entity_name = True

    def __init__(self, entry, coordinator, key):
        super().__init__(coordinator)
        self._attr_unique_id = f"{entry.entry_id}_{key}"
        self._attr_translation_key = key
        self._attr_device_info = DeviceInfo(
            identifiers={("rworker", entry.entry_id)},
            name=f"rworker ({entry.title})",
            manufacturer="rworker",
            model="Controller",
        )
