"""Render Strobes pulse events to the terminal, Claude-Code style.

Consumes the unified ``StreamEvent`` payloads emitted by the backend
(strobes/agents/streaming/events.py) and prints a live, readable transcript:
assistant text streams inline, thinking is dimmed, tool calls render as compact
cards with results, tasks/plan show a checklist, and run lifecycle shows status.
"""

from __future__ import annotations

import json
from typing import Any

from rich.console import Console, Group
from rich.json import JSON
from rich.panel import Panel
from rich.text import Text

_STATUS_ICON = {
    "created": "○",
    "started": "◐",
    "in_progress": "◐",
    "completed": "●",
    "failed": "✗",
}

_TOOL_ICON = "⚙"


class EventRenderer:
    """Stateful renderer — tracks streaming so text flows naturally."""

    def __init__(self, console: Console | None = None, show_thinking: bool = True):
        self.console = console or Console()
        self.show_thinking = show_thinking
        self._streaming: str | None = None  # "token" | "thinking" | None
        self._last_agent: str | None = None

    # ---- public entry point --------------------------------------------

    def handle(self, event: dict[str, Any]) -> None:
        """Render one event.

        ``event`` is the flat StreamEvent the PulseConsumer forwards. The
        top-level ``type`` is the event type directly (e.g. ``token``,
        ``tool``, ``message.created``). Structured data lives in ``data``
        (ephemeral ``emit_event`` path) or ``payload`` (persisted
        ``broadcast_db_event`` path).
        """
        etype = event.get("type") or ""
        base = etype.split(".")[0]  # "message.created" -> "message"
        method = getattr(self, f"_on_{base}", None)
        if method:
            method(event)
        else:
            self._end_stream()

    @staticmethod
    def _blob(event: dict[str, Any]) -> dict[str, Any]:
        """Structured fields, from ``data`` or ``payload`` (whichever is set)."""
        data = event.get("data")
        if isinstance(data, dict):
            return data
        payload = event.get("payload")
        return payload if isinstance(payload, dict) else {}

    def close_stream(self) -> None:
        self._end_stream()

    # ---- streaming text -------------------------------------------------

    def _on_token(self, event: dict[str, Any]) -> None:
        blob = self._blob(event)
        content = event.get("content") or blob.get("text") or blob.get("content") or ""
        if not content:
            return
        if self._streaming != "token":
            self._end_stream()
            self._print_agent_prefix(event)
            self._streaming = "token"
        self.console.print(content, end="", markup=False, highlight=False, soft_wrap=True)

    def _on_thinking(self, event: dict[str, Any]) -> None:
        if not self.show_thinking:
            return
        blob = self._blob(event)
        content = event.get("content") or blob.get("text") or blob.get("content") or ""
        if not content:
            return
        if self._streaming != "thinking":
            self._end_stream()
            self.console.print(Text("  thinking", style="dim italic"))
            self._streaming = "thinking"
        self.console.print(Text(content, style="dim italic"), end="", soft_wrap=True)

    def _print_agent_prefix(self, event: dict[str, Any]) -> None:
        agent = event.get("agentName") or "agent"
        self.console.print(Text(f"● {agent}  ", style="bold green"), end="")

    def _end_stream(self) -> None:
        if self._streaming is not None:
            self.console.print()  # close the open line
            self._streaming = None

    # ---- tools ----------------------------------------------------------

    def _on_tool(self, event: dict[str, Any]) -> None:
        self._end_stream()
        data = self._blob(event)
        status = data.get("status")
        name = data.get("toolName", "tool")
        if status in ("start", "local_execute"):
            args = data.get("arguments") or data.get("input") or {}
            arg_preview = self._compact(args, limit=160)
            tag = " (local)" if status == "local_execute" else ""
            self.console.print(
                Text(f"  {_TOOL_ICON} {name}{tag} ", style="cyan")
                + Text(arg_preview, style="dim")
            )
        elif status == "output":
            dur = data.get("durationMs")
            suffix = f"  [{dur}ms]" if dur is not None else ""
            result = data.get("result")
            preview = self._compact(result, limit=600)
            body = Text(preview or "(ok)", style="white")
            self.console.print(
                Panel(body, title=f"{name} → result{suffix}", title_align="left",
                      border_style="green", padding=(0, 1))
            )
            artifact = data.get("artifact")
            if artifact:
                self.console.print(Text(f"     ↳ artifact: {self._compact(artifact, 120)}", style="blue"))
        elif status == "failed":
            err = data.get("error", "tool failed")
            self.console.print(
                Panel(Text(str(err), style="red"), title=f"{name} → failed",
                      title_align="left", border_style="red", padding=(0, 1))
            )

    # ---- approvals / interrupts ----------------------------------------

    def _on_approval(self, event: dict[str, Any]) -> None:
        self._end_stream()
        data = self._blob(event)
        status = data.get("status")
        if status == "requested":
            module = data.get("module", "")
            count = data.get("targetCount")
            tail = f" · {count} target(s)" if count else ""
            self.console.print(
                Text(f"  ⚠ approval requested ({module}){tail}", style="bold yellow")
            )
            preview = data.get("preview")
            if preview:
                self.console.print(Text(f"     {preview}", style="yellow"))
        elif status in ("approved", "rejected"):
            style = "green" if status == "approved" else "red"
            self.console.print(Text(f"  · approval {status}", style=style))

    def _on_interrupt(self, event: dict[str, Any]) -> None:
        self._end_stream()
        data = self._blob(event)
        status = data.get("status")
        if status == "requested":
            self.console.print(
                Text(f"  ⏸ {data.get('title', 'input required')}", style="bold cyan")
            )
            if data.get("message"):
                self.console.print(Text(f"     {data['message']}", style="cyan"))
        elif status in ("responded", "canceled", "expired"):
            self.console.print(Text(f"  · interrupt {status}", style="dim"))

    # ---- tasks / plan ---------------------------------------------------

    def _on_task(self, event: dict[str, Any]) -> None:
        self._end_stream()
        data = self._blob(event)
        status = data.get("status", "")
        icon = _STATUS_ICON.get(status, "•")
        title = data.get("title") or data.get("taskId", "")
        style = "green" if status == "completed" else ("red" if status == "failed" else "yellow")
        line = Text(f"  {icon} ", style=style) + Text(str(title), style="bold")
        if status == "failed" and data.get("error"):
            line += Text(f"  ({data['error']})", style="red")
        self.console.print(line)

    # ---- artifacts ------------------------------------------------------

    def _on_artifact(self, event: dict[str, Any]) -> None:
        self._end_stream()
        data = self._blob(event)
        status = data.get("status")
        name = data.get("name", "artifact")
        if status == "ready":
            url = data.get("downloadUrl", "")
            size = data.get("size")
            extra = f" ({size} bytes)" if size else ""
            self.console.print(Text(f"  📄 artifact ready: {name}{extra}", style="blue"))
            if url:
                self.console.print(Text(f"     {url}", style="dim blue"))
        elif status == "error":
            self.console.print(Text(f"  📄 artifact error: {name} — {data.get('error','')}", style="red"))

    # ---- tables ---------------------------------------------------------

    def _on_table(self, event: dict[str, Any]) -> None:
        self._end_stream()
        data = self._blob(event)
        self.console.print(
            Text(f"  ▦ table {data.get('status','')}: {data.get('tableName','')} "
                 f"({data.get('rowCount', 0)} rows)", style="magenta")
        )

    # ---- guardrails -----------------------------------------------------

    def _on_guardrail(self, event: dict[str, Any]) -> None:
        self._end_stream()
        data = self._blob(event)
        msg = data.get("message", "guardrail triggered")
        sev = data.get("severity", "")
        self.console.print(Text(f"  ⚠ guardrail [{data.get('status','')}] {msg} {sev}", style="yellow"))

    # ---- system lifecycle ----------------------------------------------

    def _on_system(self, event: dict[str, Any]) -> None:
        self._end_stream()
        data = self._blob(event)
        kind = data.get("type", "")
        if kind == "run.started":
            agents = data.get("selectedAgents") or []
            tail = f"  · agents: {', '.join(agents)}" if agents else ""
            self.console.print(Text(f"\n▶ run started{tail}", style="bold blue"))
        elif kind == "run.completed":
            metrics = data.get("metrics") or {}
            tok = metrics.get("total_tokens")
            credits = metrics.get("credits_used")
            bits = []
            if tok is not None:
                bits.append(f"{tok} tokens")
            if credits is not None:
                bits.append(f"{credits} credits")
            tail = f"  · {', '.join(bits)}" if bits else ""
            self.console.print(Text(f"✔ run completed{tail}\n", style="bold green"))
        elif kind == "run.failed":
            self.console.print(Text(f"✗ run failed: {data.get('error','')}\n", style="bold red"))
        elif kind == "credentials.missing":
            creds = data.get("missing_credentials") or []
            names = ", ".join(c.get("display_name", c.get("key", "?")) for c in creds)
            self.console.print(Text(f"🔑 credentials required: {names}", style="yellow"))
            self.console.print(Text("   provide them in the Strobes UI, then resend.", style="dim"))
        elif kind == "message.dequeued":
            self.console.print(Text("  · message dequeued", style="dim"))
        elif kind == "workspace.finalized":
            self.console.print(Text("  · workspace finalized", style="dim"))
        else:
            self.console.print(Text(f"  · {kind} {self._compact(data, 120)}", style="dim"))

    # ---- message / run lifecycle (persisted, dotted types) -------------

    def _on_message(self, event: dict[str, Any]) -> None:
        # message.created / message.queued / message.completed
        self._end_stream()
        kind = (event.get("type") or "").split(".", 1)[-1]
        if kind == "queued":
            self.console.print(Text("  · queued — waiting for a worker", style="dim"))
        # message.created / completed carry no extra UI value here.

    def _on_run(self, event: dict[str, Any]) -> None:
        # run.created / run.started / run.completed / run.failed (top-level dotted)
        kind = (event.get("type") or "").split(".", 1)[-1]
        data = self._blob(event)
        self._end_stream()
        if kind == "started":
            self.console.print(Text("\n▶ run started", style="bold blue"))
        elif kind == "completed":
            self.console.print(Text("✔ run completed\n", style="bold green"))
        elif kind == "failed":
            self.console.print(Text(f"✗ run failed: {data.get('error','')}\n", style="bold red"))

    # ---- helpers --------------------------------------------------------

    @staticmethod
    def _compact(value: Any, limit: int = 200) -> str:
        if value is None:
            return ""
        if isinstance(value, (dict, list)):
            try:
                text = json.dumps(value, default=str)
            except Exception:  # noqa: BLE001
                text = str(value)
        else:
            text = str(value)
        text = text.replace("\n", " ")
        return text if len(text) <= limit else text[: limit - 1] + "…"


def print_banner(console: Console, profile_name: str, base_url: str, workspace: str | None) -> None:
    body = Group(
        Text("Strobes Agents AI", style="bold cyan"),
        Text(f"profile: {profile_name}   ·   {base_url}", style="dim"),
        Text(f"workspace: {workspace or '(none — will create one)'}", style="dim"),
    )
    console.print(Panel(body, border_style="cyan", padding=(0, 2)))
