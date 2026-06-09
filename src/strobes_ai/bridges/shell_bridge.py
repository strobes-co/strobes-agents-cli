"""Shell bridge daemon — makes the local machine a remote agent's sandbox.

Connects to ``ws/<org_id>/shell-bridge/?api_key=<key>&bridge_id=<id>&workspace_id=<ws>``
and serves the command set the backend routes to it (shell_execute,
shell_execute_code, file_read/write/list) plus interactive PTY sessions.
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
from ..local.shell import LocalShell
from .pty_session import PtySession


class ShellBridgeDaemon:
    def __init__(
        self,
        profile: Profile,
        bridge_id: str,
        shell_name: str = "",
        workspace_id: Optional[str] = None,
        workdir: Optional[str] = None,
        console: Optional[Console] = None,
    ):
        self.profile = profile
        self.bridge_id = bridge_id
        self.shell_name = shell_name or f"{__import__('socket').gethostname()} (CLI)"
        self.workspace_id = workspace_id or profile.workspace_id
        self.shell = LocalShell(workdir=workdir)
        self.console = console or Console()
        self._ws: Optional[websockets.WebSocketClientProtocol] = None
        self._ptys: dict[str, PtySession] = {}
        self._stop = asyncio.Event()

    @property
    def url(self) -> str:
        from urllib.parse import urlencode

        q = {"api_key": self.profile.master_key, "bridge_id": self.bridge_id}
        if self.workspace_id:
            q["workspace_id"] = self.workspace_id
        return f"{ws_base(self.profile)}/ws/{self.profile.org_id}/shell-bridge/?{urlencode(q)}"

    async def run(self) -> None:
        """Connect and serve, with automatic reconnect."""
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
                        f"[green]● shell bridge connected[/green] "
                        f"[dim]({self.shell_name}, cwd={self.shell.workdir})[/dim]"
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
                    f"[yellow]shell bridge disconnected ({exc}); "
                    f"reconnecting in {backoff:.0f}s[/yellow]"
                )
                await asyncio.sleep(backoff)
                backoff = min(backoff * 2, 30)

    def stop(self) -> None:
        self._stop.set()
        for pty in list(self._ptys.values()):
            pty.close()

    # ---- protocol ------------------------------------------------------

    async def _send(self, message: dict[str, Any]) -> None:
        if self._ws:
            await self._ws.send(json.dumps(message))

    async def _identify(self) -> None:
        await self._send(
            {"type": "identify", "data": {"shell_name": self.shell_name,
                                          "version": "0.1.0", "platform": "cli"}}
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
            await self._dispatch(msg)

    async def _dispatch(self, msg: dict[str, Any]) -> None:
        mtype = msg.get("type")
        if mtype == "command":
            await self._handle_command(msg)
        elif mtype == "identify_ack":
            self.bridge_id = msg.get("data", {}).get("bridge_id", self.bridge_id)
        elif mtype == "pong":
            pass
        elif mtype in ("pty_open", "pty_input", "pty_resize", "pty_close"):
            await self._handle_pty(mtype, msg)

    async def _handle_command(self, msg: dict[str, Any]) -> None:
        command = msg.get("command")
        params = msg.get("params") or {}
        request_id = msg.get("request_id")
        data = await self._execute(command, params)
        await self._send({"type": "response", "request_id": request_id, "data": data})

    async def _execute(self, command: str, params: dict[str, Any]) -> dict[str, Any]:
        try:
            if command == "shell_execute":
                return await asyncio.to_thread(
                    self.shell.execute, params.get("command", ""),
                    params.get("timeout", 60),
                )
            if command == "shell_execute_code":
                return await asyncio.to_thread(
                    self.shell.execute_code,
                    params.get("code", ""),
                    params.get("language", "python"),
                    params.get("timeout", 120),
                )
            if command == "file_write":
                return self.shell.write_file(params.get("path", ""), params.get("content", ""))
            if command == "file_read":
                return self.shell.read_file(params.get("path", ""))
            if command == "file_list":
                return self.shell.list_files(params.get("directory", "."))
            return {"success": False, "error": f"unknown command: {command}"}
        except Exception as exc:  # noqa: BLE001
            return {"success": False, "error": str(exc)}

    # ---- PTY -----------------------------------------------------------

    async def _handle_pty(self, mtype: str, msg: dict[str, Any]) -> None:
        session_id = msg.get("session_id", "")
        if not session_id:
            return
        if mtype == "pty_open":
            try:
                self._ptys[session_id] = PtySession(
                    session_id,
                    int(msg.get("cols", 80)),
                    int(msg.get("rows", 24)),
                    on_output=self._pty_output,
                    on_closed=self._pty_closed,
                )
            except Exception as exc:  # noqa: BLE001
                await self._pty_output(session_id, f"\r\n[pty error: {exc}]\r\n")
                await self._pty_closed(session_id)
        elif mtype == "pty_input":
            pty = self._ptys.get(session_id)
            if pty:
                pty.write(msg.get("data", ""))
        elif mtype == "pty_resize":
            pty = self._ptys.get(session_id)
            if pty:
                pty.resize(int(msg.get("cols", 80)), int(msg.get("rows", 24)))
        elif mtype == "pty_close":
            pty = self._ptys.pop(session_id, None)
            if pty:
                pty.close()

    async def _pty_output(self, session_id: str, data: str) -> None:
        await self._send({"type": "pty_output", "session_id": session_id, "data": data})

    async def _pty_closed(self, session_id: str) -> None:
        self._ptys.pop(session_id, None)
        await self._send({"type": "pty_closed", "session_id": session_id})
