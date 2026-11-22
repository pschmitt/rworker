"""Authenticated controller transport."""

import asyncio
import json
from contextlib import aclosing

from aiohttp import ClientSession, ClientTimeout, WSMsgType


class Controller:
    """Receive inventory snapshots and submit explicitly supported actions."""

    def __init__(self, session: ClientSession, url: str, token: str) -> None:
        self.session = session
        self.url = url
        self.headers = {"Authorization": f"Bearer {token}"}

    async def snapshots(self):
        """Yield full snapshots. Blank frames are controller heartbeats."""
        async with self.session.ws_connect(
            f"{self.url}/v1/automation/workers",
            headers=self.headers,
        ) as socket:
            while True:
                message = await socket.receive(timeout=20)
                if message.type != WSMsgType.TEXT:
                    raise ConnectionError("Inventory stream closed")
                if not message.data.strip():
                    continue
                rows = json.loads(message.data)
                if not isinstance(rows, list) or any(
                    not isinstance(row, dict) or not isinstance(row.get("name"), str)
                    for row in rows
                ):
                    raise ValueError("Invalid inventory")
                yield {row["name"]: row for row in rows}

    async def probe(self):
        """Confirm authentication and the initial snapshot."""
        async with asyncio.timeout(15):
            async with self.snapshots_context() as snapshots:
                return await anext(snapshots)

    def snapshots_context(self):
        """Close the stream promptly when leaving the consumer."""
        return aclosing(self.snapshots())

    async def action(self, action, fields):
        """Send a command once without retrying mutations."""
        async with self.session.post(
            f"{self.url}/v1/automation/action",
            headers=self.headers,
            json={"action": action, **fields},
            timeout=ClientTimeout(total=3610 if action == "exec" else 910),
            allow_redirects=False,
        ) as response:
            response.raise_for_status()
            if response.status != 200:
                raise ConnectionError("Unexpected controller status")
            return await response.json()
