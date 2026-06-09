"""Run the local bridges (shell + optional browser) as a long-lived daemon.

Used both standalone (``strobes-ai-bridge``) and embedded inside the chat
command so the bound workspace always has a live local sandbox + browser.
"""

from __future__ import annotations

import asyncio
import signal
import uuid
from typing import Optional

from rich.console import Console

from ..api import StrobesClient
from ..config import Config, Profile
from .browser_bridge import BrowserBridgeDaemon
from .shell_bridge import ShellBridgeDaemon


def ensure_bridge_shell(profile: Profile, console: Console) -> str:
    """Make sure the profile has a registered bridge Shell; return its bridge_id."""
    if profile.shell_bridge_id:
        return profile.shell_bridge_id
    with StrobesClient(profile) as client:
        name = f"{__import__('socket').gethostname()}-cli"
        shell = client.register_bridge_shell(name=name)
        bridge_id = shell.get("bridge_id") or str(uuid.uuid4())
        profile.shell_bridge_id = bridge_id
        # Attach to the bound workspace so the agent uses this sandbox.
        if profile.workspace_id and shell.get("id"):
            try:
                client.attach_shell_to_workspace(profile.workspace_id, shell["id"])
            except Exception as exc:  # noqa: BLE001
                console.print(f"[yellow]could not attach shell to workspace: {exc}[/yellow]")
    return profile.shell_bridge_id


async def run_bridges(
    profile: Profile,
    config: Optional[Config] = None,
    with_browser: bool = True,
    headless: bool = False,
    workdir: Optional[str] = None,
    console: Optional[Console] = None,
) -> None:
    console = console or Console()
    bridge_id = ensure_bridge_shell(profile, console)
    if config is not None:
        config.save()

    shell = ShellBridgeDaemon(
        profile, bridge_id=bridge_id, workspace_id=profile.workspace_id,
        workdir=workdir, console=console,
    )
    daemons: list = [shell]
    if with_browser:
        browser = BrowserBridgeDaemon(
            profile, browser_id=profile.browser_id or f"strobes-cli-{uuid.uuid4().hex[:12]}",
            workspace_id=profile.workspace_id, headless=headless, console=console,
        )
        daemons.append(browser)

    tasks = [asyncio.create_task(d.run()) for d in daemons]

    stop = asyncio.Event()

    def _signal() -> None:
        for d in daemons:
            d.stop()
        stop.set()

    loop = asyncio.get_event_loop()
    for sig in (signal.SIGINT, signal.SIGTERM):
        try:
            loop.add_signal_handler(sig, _signal)
        except (NotImplementedError, ValueError):
            pass

    await asyncio.gather(*tasks, return_exceptions=True)


def main() -> None:
    """Console entry point: ``strobes-ai-bridge``."""
    import argparse

    parser = argparse.ArgumentParser(description="Run the Strobes local bridges.")
    parser.add_argument("--profile", default=None, help="config profile name")
    parser.add_argument("--no-browser", action="store_true", help="shell bridge only")
    parser.add_argument("--headless", action="store_true", help="headless browser")
    parser.add_argument("--workdir", default=None, help="sandbox working directory")
    args = parser.parse_args()

    console = Console()
    config = Config.load()
    if args.profile:
        config.use(args.profile)
    profile = config.current()
    if not profile.is_complete():
        console.print("[red]profile incomplete — run `strobes-ai login` first.[/red]")
        raise SystemExit(1)

    try:
        asyncio.run(
            run_bridges(
                profile, config, with_browser=not args.no_browser,
                headless=args.headless, workdir=args.workdir, console=console,
            )
        )
    except KeyboardInterrupt:
        console.print("\n[dim]bridges stopped.[/dim]")
