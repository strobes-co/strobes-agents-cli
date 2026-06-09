"""Interactive chat REPL — the Claude-Code-style front door.

Streams an agent run live, executes tools on the local machine (CLI_LOCAL
mode), and handles human-in-the-loop approvals and interrupts inline.
"""

from __future__ import annotations

import asyncio
from typing import Optional

from prompt_toolkit import PromptSession
from prompt_toolkit.history import FileHistory
from prompt_toolkit.patch_stdout import patch_stdout
from rich.console import Console

from .config import Profile, config_dir
from .local.dispatch import LocalToolRouter
from .render import EventRenderer, print_banner
from .ws.pulse import PulseClient

_SLASH_HELP = """\
commands:
  /stop        cancel the active run
  /clear       clear the screen
  /thinking    toggle showing the agent's thinking
  /help        show this help
  /exit, /quit leave the chat
Type anything else to send a message. Ctrl-C interrupts a run; Ctrl-D exits.
"""


class ConsoleInteractor:
    """Blocking approval / interrupt prompts driven from the terminal."""

    def __init__(self, console: Console, auto_approve: bool = False):
        self.console = console
        self.auto_approve = auto_approve

    async def approve(self, approval_id: str, data: dict) -> str:
        # The request itself is already rendered by EventRenderer._on_approval;
        # here we only collect the decision.
        if self.auto_approve:
            self.console.print("[dim]  · auto-approved[/dim]")
            return "approved"
        answer = (await asyncio.to_thread(input, "  approve? [y/N] ")).strip().lower()
        return "approved" if answer in ("y", "yes") else "rejected"

    async def interrupt(self, interrupt_id: str, data: dict) -> dict:
        # Title/message already rendered; just collect the form fields.
        schema = data.get("formSchema") or {}
        fields = schema.get("fields") or [{"key": "value", "label": "value"}]
        response: dict = {}
        for field in fields:
            key = field.get("key", "value")
            label = field.get("label", key)
            required = field.get("required", False)
            prompt = f"  {label}{' *' if required else ''}: "
            value = (await asyncio.to_thread(input, prompt)).strip()
            response[key] = value
        return response


async def chat_loop(
    profile: Profile,
    profile_name: str,
    thread_id: str,
    console: Optional[Console] = None,
    local_mode: bool = True,
    auto_approve: bool = False,
    headless: bool = False,
    llm_model: Optional[int] = None,
    workdir: Optional[str] = None,
) -> None:
    console = console or Console()
    renderer = EventRenderer(console=console)
    interactor = ConsoleInteractor(console, auto_approve=auto_approve)
    router = LocalToolRouter(headless=headless)
    if workdir:
        from .local.shell import LocalShell

        router.shell = LocalShell(workdir=workdir)

    client = PulseClient(
        profile, thread_id, renderer, router=router, interactor=interactor,
        local_mode=local_mode, llm_model=llm_model,
    )

    print_banner(console, profile_name, profile.base_url, profile.workspace_id)
    console.print(f"[dim]thread {thread_id} · local mode {'on' if local_mode else 'off'} · "
                  f"/help for commands[/dim]\n")

    await client.connect()
    history = FileHistory(str(config_dir() / "history"))
    session: PromptSession = PromptSession(history=history)

    try:
        while True:
            try:
                with patch_stdout():
                    text = await session.prompt_async("› ")
            except (EOFError, KeyboardInterrupt):
                break
            text = text.strip()
            if not text:
                continue
            if text in ("/exit", "/quit"):
                break
            if text == "/help":
                console.print(_SLASH_HELP)
                continue
            if text == "/clear":
                console.clear()
                continue
            if text == "/thinking":
                renderer.show_thinking = not renderer.show_thinking
                console.print(f"[dim]thinking {'shown' if renderer.show_thinking else 'hidden'}[/dim]")
                continue
            if text == "/stop":
                await client.cancel()
                continue

            await client.send_user_message(text)
            try:
                await client.wait_idle()
            except KeyboardInterrupt:
                await client.cancel()
                console.print("\n[yellow]· run interrupted[/yellow]")
                await client.wait_idle()
    finally:
        renderer.close_stream()
        await client.close()
        console.print("\n[dim]session closed.[/dim]")
