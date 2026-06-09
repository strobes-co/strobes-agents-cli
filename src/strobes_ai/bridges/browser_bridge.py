"""Browser bridge daemon — makes the local browser a remote agent's browser.

Connects to ``ws/browser-bridge/?org_id=<org>&api_key=<key>&browser_id=<id>&workspace_id=<ws>``
and serves the ``browser_*`` command set via Playwright on the local machine.
"""

from __future__ import annotations

import asyncio
import json
import time
from typing import Any, Optional

import websockets
from rich.console import Console

from ..api import ws_base
from ..config import Profile
from ..local.browser import BrowserUnavailable, LocalBrowser


class BrowserBridgeDaemon:
    def __init__(
        self,
        profile: Profile,
        browser_id: str,
        browser_name: str = "",
        workspace_id: Optional[str] = None,
        headless: bool = False,
        console: Optional[Console] = None,
    ):
        self.profile = profile
        self.browser_id = browser_id
        self.browser_name = browser_name or f"{__import__('socket').gethostname()} (CLI)"
        self.workspace_id = workspace_id or profile.workspace_id
        self.browser = LocalBrowser(headless=headless)
        self.console = console or Console()
        self._ws: Optional[websockets.WebSocketClientProtocol] = None
        self._stop = asyncio.Event()

    @property
    def url(self) -> str:
        from urllib.parse import urlencode

        q = {
            "api_key": self.profile.master_key,
            "org_id": self.profile.org_id,
            "browser_id": self.browser_id,
        }
        if self.workspace_id:
            q["workspace_id"] = self.workspace_id
        return f"{ws_base(self.profile)}/ws/browser-bridge/?{urlencode(q)}"

    async def run(self) -> None:
        backoff = 1.0
        while not self._stop.is_set():
            try:
                async with websockets.connect(
                    self.url, max_size=64 * 1024 * 1024, ping_interval=None
                ) as ws:
                    self._ws = ws
                    backoff = 1.0
                    await self._identify()
                    self.console.print(
                        f"[green]● browser bridge connected[/green] "
                        f"[dim]({self.browser_name})[/dim]"
                    )
                    ping = asyncio.create_task(self._ping_loop())
                    try:
                        await self._serve()
                    finally:
                        ping.cancel()
            except (OSError, websockets.WebSocketException) as exc:
                if self._stop.is_set():
                    break
                self.console.print(
                    f"[yellow]browser bridge disconnected ({exc}); "
                    f"reconnecting in {backoff:.0f}s[/yellow]"
                )
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30)
        await self.browser.close()

    def stop(self) -> None:
        self._stop.set()

    # ---- protocol ------------------------------------------------------

    async def _send(self, message: dict[str, Any]) -> None:
        if self._ws:
            await self._ws.send(json.dumps(message))

    async def _identify(self) -> None:
        caps = list({
            "browser_init", "browser_navigate", "browser_snapshot", "browser_click",
            "browser_type", "browser_scroll", "browser_screenshot",
            "browser_execute_script", "browser_get_cookies",
        })
        await self._send(
            {"type": "identify", "data": {"browser_name": self.browser_name,
                                          "version": "0.1.0", "engine": "playwright-chromium",
                                          "capabilities": caps}}
        )

    async def _ping_loop(self) -> None:
        try:
            while True:
                await asyncio.sleep(30)
                await self._send({"type": "ping", "timestamp": time.time()})
        except asyncio.CancelledError:
            return

    async def _serve(self) -> None:
        assert self._ws is not None
        async for raw in self._ws:
            try:
                msg = json.loads(raw)
            except json.JSONDecodeError:
                continue
            mtype = msg.get("type")
            if mtype == "command":
                await self._handle_command(msg)
            elif mtype == "identify_ack":
                self.browser_id = msg.get("data", {}).get("browser_id", self.browser_id)

    async def _handle_command(self, msg: dict[str, Any]) -> None:
        command = msg.get("command", "")
        params = msg.get("params") or {}
        request_id = msg.get("request_id")
        try:
            data = await self.browser.handle(command, params)
        except BrowserUnavailable as exc:
            data = {"success": False, "error": str(exc)}
        except Exception as exc:  # noqa: BLE001
            data = {"success": False, "error": str(exc)}
        await self._send({"type": "response", "request_id": request_id, "data": data})
