"""Share controller health without credentials or worker metadata."""


async def async_get_config_entry_diagnostics(hass, entry):
    """Return only aggregate inventory and connection health."""
    runtime = entry.runtime_data
    inventory = runtime.coordinator.data
    return {
        "connected": runtime.connected.data,
        "last_snapshot": inventory.received.isoformat(),
        "worker_count": len(inventory.workers),
        "inventory_available": runtime.coordinator.last_update_success,
    }
