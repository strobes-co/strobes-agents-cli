"""Pulse chat WebSocket client.

Connects to ``ws/<org_id>/pulse/<thread_id>/?api_key=<key>`` and speaks the
PulseConsumer protocol (strobes/agents/consumers.py):

Client → server:
  send_message       {text, attachments?, context}
  run.cancel         {run_id?}
  approval.response  {approval_id, decision}
  interrupt.response {interrupt_id, response_data}
  tool.local_result  {payload: {request_id, tool_name, output, exit_code, duration_ms}}
  tool.local_error   {payload: {request_id, tool_name, error, error_type}}
  ping               {timestamp}

Server → client:
  pulse_event        {data: <StreamEvent>}   — streamed transcript
  message_sent       {run_id, message_id, queued}
  error              {code, detail, ...}
  pong

When ``context.client_type == "cli"`` the backend runs in CLI_LOCAL mode and
emits ``tool.local_execute`` events instead of running code/browser tools in the
cloud. This client executes those locally via :class:`LocalToolRouter` and
returns the result — making the user's machine the sandbox and browser.
"""

from __future__ import annotations

import asyncio
import json
import time
from typing import Any, Optional, Protocol

import websockets

from ..api import ws_url
from ..config import Profile
from ..local.dispatch import LocalToolRouter
from ..render import EventRenderer


class Interactor(Protocol):
    """Handles blocking human-in-the-loop prompts (approvals / interrupts)."""

    async def approve(self, approval_id: str, data: dict) -> str:  # "approved" | "rejected"
        ...

    async def interrupt(self, interrupt_id: str, data: dict) -> dict:
        ...


class AutoInteractor:
    """Non-interactive default: reject approvals, cancel interrupts."""

    def __init__(self, auto_approve: bool = False):
        self.auto_approve = auto_approve

    async def approve(self, approval_id: str, data: dict) -> str:
        return "approved" if self.auto_approve else "rejected"

    async def interrupt(self, interrupt_id: str, data: dict) -> dict:
        return {}


class PulseClient:
    """Async client for one pulse thread."""

    def __init__(
        self,
        profile: Profile,
        thread_id: str,
        renderer: EventRenderer,
        router: Optional[LocalToolRouter] = None,
        interactor: Optional[Interactor] = None,
        local_mode: bool = True,
        llm_model: Optional[int] = None,
    ):
        self.profile = profile
        self.thread_id = thread_id
        self.renderer = renderer
        self.router = router or LocalToolRouter()
        self.interactor = interactor or AutoInteractor()
        self.local_mode = local_mode
        self.llm_model = llm_model

        self._ws: Optional[websockets.WebSocketClientProtocol] = None
        self._recv_task: Optional[asyncio.Task] = None
        self._ping_task: Optional[asyncio.Task] = None
        self.idle = asyncio.Event()
        self.idle.set()
        self._active_run_id: Optional[str] = None
        self._closed = False

    # ---- connection ----------------------------------------------------

    @property
    def url(self) -> str:
        return ws_url(
            self.profile,
            f"/ws/{self.profile.org_id}/pulse/{self.thread_id}/",
        )

    async def connect(self) -> None:
        self._ws = await websockets.connect(
            self.url, max_size=64 * 1024 * 1024, ping_interval=None
        )
        self._recv_task = asyncio.create_task(self._receive_loop())
        self._ping_task = asyncio.create_task(self._ping_loop())

    async def close(self) -> None:
        self._closed = True
        for task in (self._ping_task, self._recv_task):
            if task:
                task.cancel()
        if self._ws:
            await self._ws.close()
        await self.router.close()

    # ---- outbound ------------------------------------------------------

    async def _send(self, message: dict[str, Any]) -> None:
        if not self._ws:
            raise RuntimeError("not connected")
        await self._ws.send(json.dumps(message))

    async def send_user_message(self, text: str) -> None:
        context = {
            "client_type": "cli" if self.local_mode else "web",
            "workspace_id": self.profile.workspace_id,
        }
        if self.llm_model is not None:
            context["llm_model"] = self.llm_model
        self.idle.clear()
        await self._send({"type": "send_message", "text": text, "context": context})

    async def cancel(self) -> None:
        msg: dict[str, Any] = {"type": "run.cancel"}
        if self._active_run_id:
            msg["run_id"] = self._active_run_id
        await self._send(msg)

    async def _ping_loop(self) -> None:
        try:
            while not self._closed:
                await asyncio.sleep(30)
                try:
                    await self._send({"type": "ping", "timestamp": time.time()})
                except Exception:  # noqa: BLE001
                    return
        except asyncio.CancelledError:
            return

    # ---- inbound -------------------------------------------------------

    async def _receive_loop(self) -> None:
        try:
            assert self._ws is not None
            async for raw in self._ws:
                try:
                    msg = json.loads(raw)
                except json.JSONDecodeError:
                    continue
                await self._dispatch(msg)
        except (asyncio.CancelledError, websockets.ConnectionClosed):
            pass
        except Exception as exc:  # noqa: BLE001
            self.renderer.console.print(f"[red]connection error: {exc}[/red]")
        finally:
            self.idle.set()

    # Top-level frames that are control messages, not stream events.
    _CONTROL = {"message_sent", "run_started", "run_already_started", "pong", "error"}

    async def _dispatch(self, msg: dict[str, Any]) -> None:
        mtype = msg.get("type")
        if mtype == "message_sent":
            self._active_run_id = msg.get("run_id")
            if msg.get("queued"):
                self.renderer.console.print("[dim]· message queued[/dim]")
        elif mtype in ("run_started", "run_already_started"):
            self._active_run_id = msg.get("run_id", self._active_run_id)
        elif mtype == "pong":
            pass
        elif mtype == "error":
            self.renderer.close_stream()
            detail = msg.get("detail") or msg.get("error") or msg.get("reason") or msg
            self.renderer.console.print(f"[bold red]error:[/bold red] {detail}")
            self.idle.set()
        elif mtype == "pulse_event":
            # Defensive: handle the wrapped form too (data/event key).
            await self._handle_event(msg.get("data") or msg.get("event") or {})
        else:
            # Anything else IS a stream event — the consumer forwards the
            # flat StreamEvent directly (type = token/thinking/tool/...).
            await self._handle_event(msg)

    @staticmethod
    def _blob(event: dict[str, Any]) -> dict[str, Any]:
        """Structured fields live in ``data`` (ephemeral) or ``payload`` (persisted)."""
        data = event.get("data")
        if isinstance(data, dict):
            return data
        payload = event.get("payload")
        return payload if isinstance(payload, dict) else {}

    async def _handle_event(self, event: dict[str, Any]) -> None:
        etype = event.get("type") or ""
        data = self._blob(event)
        status = data.get("status")

        # CLI_LOCAL tool execution — the user's machine is the sandbox/browser.
        if etype == "tool" and status == "local_execute":
            self.renderer.handle(event)  # show the call
            await self._run_local_tool(data)
            return

        # Blocking approval.
        if etype == "approval" and status == "requested":
            self.renderer.handle(event)
            approval_id = data.get("approvalId") or data.get("approval_id")
            decision = await self.interactor.approve(approval_id, data)
            await self._send(
                {"type": "approval.response", "approval_id": approval_id,
                 "decision": decision}
            )
            self.renderer.console.print(f"[dim]· approval {decision}[/dim]")
            return

        # Blocking interrupt (form / OTP / handover).
        if etype == "interrupt" and status == "requested":
            self.renderer.handle(event)
            interrupt_id = data.get("interruptId") or data.get("interrupt_id")
            response = await self.interactor.interrupt(interrupt_id, data)
            await self._send(
                {"type": "interrupt.response", "interrupt_id": interrupt_id,
                 "response_data": response}
            )
            return

        # Run lifecycle → toggle idle so the REPL knows when to re-prompt.
        # Completion shows up either as a dotted top-level type (run.completed)
        # or as a system event whose blob.type is run.completed/failed.
        terminal = etype in ("run.completed", "run.failed") or (
            etype == "system" and data.get("type") in ("run.completed", "run.failed")
        )
        self.renderer.handle(event)
        if terminal:
            self.idle.set()

    async def _run_local_tool(self, data: dict[str, Any]) -> None:
        request_id = data.get("requestId")
        tool_name = data.get("toolName", "")
        tool_input = data.get("input") or {}
        start = time.monotonic()
        result = await self.router.execute(tool_name, tool_input)
        duration_ms = int((time.monotonic() - start) * 1000)

        if result.get("error"):
            await self._send(
                {
                    "type": "tool.local_error",
                    "payload": {
                        "request_id": request_id,
                        "tool_name": tool_name,
                        "error": result["error"],
                        "error_type": result.get("error_type", "Error"),
                    },
                }
            )
            self.renderer.console.print(
                f"[red]  ↳ {tool_name} failed: {result['error']}[/red]"
            )
        else:
            await self._send(
                {
                    "type": "tool.local_result",
                    "payload": {
                        "request_id": request_id,
                        "tool_name": tool_name,
                        "output": result.get("output", ""),
                        "exit_code": result.get("exit_code"),
                        "duration_ms": duration_ms,
                    },
                }
            )
            code = result.get("exit_code")
            tail = f" (exit {code})" if code not in (0, None) else ""
            self.renderer.console.print(
                f"[dim]  ↳ {tool_name} done in {duration_ms}ms{tail}[/dim]"
            )

    # ---- convenience ---------------------------------------------------

    async def wait_idle(self) -> None:
        await self.idle.wait()
