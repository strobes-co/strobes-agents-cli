"""``strobes-ai`` — the command-line entry point.

    strobes-ai login                 configure a deployment + MasterKey
    strobes-ai status                show the active binding & connectivity
    strobes-ai workspaces            list remote workspaces
    strobes-ai threads               list your chat threads
    strobes-ai bind [WORKSPACE_ID]   bind a workspace (interactive if omitted)
    strobes-ai chat                  start an interactive agent session
    strobes-ai bridge                run the local shell + browser bridges
    strobes-ai profiles / use NAME   manage connection profiles
"""

from __future__ import annotations

import asyncio
from typing import Optional

import typer
from rich.console import Console
from rich.table import Table

from .api import StrobesAPIError, StrobesClient
from .config import Config, redact

app = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="Strobes Agents AI — a local, terminal-native client for remote Strobes workspaces.",
)
console = Console()


def _load(profile: Optional[str]) -> tuple[Config, "object"]:
    config = Config.load()
    if profile:
        config.use(profile)
    return config, config.current()


def _require_complete(prof) -> None:
    if not prof.is_complete():
        console.print("[red]No active profile. Run [bold]strobes-ai login[/bold] first.[/red]")
        raise typer.Exit(1)


# ── login / status / profiles ───────────────────────────────────────────


@app.command()
def login(
    base_url: Optional[str] = typer.Option(None, help="Strobes base URL, e.g. https://app.strobes.co"),
    org_id: Optional[str] = typer.Option(None, help="Organization UUID"),
    master_key: Optional[str] = typer.Option(None, help="MasterKey token (40-char hex)"),
    deployment: Optional[str] = typer.Option(None, help="saas | enterprise"),
    profile: str = typer.Option("default", help="profile name to write"),
):
    """Configure a deployment binding (base URL, org, MasterKey)."""
    config = Config.load()
    config.use(profile)
    prof = config.profile(profile)

    prof.base_url = base_url or typer.prompt("Base URL", default=prof.base_url or "https://app.strobes.co")
    prof.org_id = org_id or typer.prompt("Organization UUID", default=prof.org_id or "")
    prof.master_key = master_key or typer.prompt("MasterKey token", default=prof.master_key or "", hide_input=True)
    prof.deployment = deployment or typer.prompt("Deployment (saas/enterprise)", default=prof.deployment or "saas")
    config.current_profile = profile
    config.save()

    console.print("[dim]verifying credentials…[/dim]")
    try:
        with StrobesClient(prof) as client:
            client.ping()
        console.print(f"[green]✔ logged in[/green] to {prof.base_url} (org {prof.org_id})")
    except StrobesAPIError as exc:
        console.print(f"[yellow]saved, but verification failed:[/yellow] {exc}")
        raise typer.Exit(1)


@app.command()
def status(profile: Optional[str] = typer.Option(None)):
    """Show the active profile and check connectivity."""
    config, prof = _load(profile)
    table = Table(show_header=False, box=None)
    table.add_row("profile", config.current_profile)
    table.add_row("base_url", prof.base_url or "[dim](unset)[/dim]")
    table.add_row("org_id", prof.org_id or "[dim](unset)[/dim]")
    table.add_row("master_key", redact(prof.master_key))
    table.add_row("deployment", prof.deployment)
    table.add_row("workspace", prof.workspace_id or "[dim](none)[/dim]")
    table.add_row("thread", prof.thread_id or "[dim](none)[/dim]")
    console.print(table)
    if prof.is_complete():
        try:
            with StrobesClient(prof) as client:
                client.ping()
            console.print("[green]✔ connection OK[/green]")
        except StrobesAPIError as exc:
            console.print(f"[red]✗ connection failed:[/red] {exc}")


@app.command()
def profiles():
    """List configured profiles."""
    config = Config.load()
    table = Table("profile", "base_url", "org", "workspace")
    for name, p in config.profiles.items():
        marker = "→ " if name == config.current_profile else "  "
        table.add_row(marker + name, p.base_url or "-", p.org_id[:8] or "-", (p.workspace_id or "-")[:8])
    console.print(table)


@app.command()
def use(name: str):
    """Switch the active profile."""
    config = Config.load()
    config.use(name)
    config.save()
    console.print(f"[green]now using profile[/green] {name}")


# ── discovery ────────────────────────────────────────────────────────────


@app.command()
def workspaces(profile: Optional[str] = typer.Option(None)):
    """List remote workspaces."""
    config, prof = _load(profile)
    _require_complete(prof)
    try:
        with StrobesClient(prof) as client:
            rows = client.list_workspaces()
    except StrobesAPIError as exc:
        console.print(f"[red]{exc}[/red]")
        raise typer.Exit(1)
    table = Table("id", "name", "status", "created")
    for w in rows:
        bound = " [green]●[/green]" if w["id"] == prof.workspace_id else ""
        table.add_row(w["id"], (w.get("name") or "") + bound, w.get("status", ""), (w.get("created_at") or "")[:19])
    console.print(table if rows else "[dim]no workspaces[/dim]")


@app.command()
def threads(profile: Optional[str] = typer.Option(None)):
    """List your chat threads."""
    config, prof = _load(profile)
    _require_complete(prof)
    try:
        with StrobesClient(prof) as client:
            rows = client.list_threads()
    except StrobesAPIError as exc:
        console.print(f"[red]{exc}[/red]")
        raise typer.Exit(1)
    table = Table("id", "title", "status", "last message")
    for t in rows:
        bound = " [green]●[/green]" if t["id"] == prof.thread_id else ""
        table.add_row(t["id"], (t.get("title") or "(untitled)") + bound, t.get("status", ""),
                      (t.get("last_message") or "")[:50])
    console.print(table if rows else "[dim]no threads[/dim]")


# ── bind ─────────────────────────────────────────────────────────────────


@app.command()
def bind(
    workspace_id: Optional[str] = typer.Argument(None, help="workspace UUID (interactive if omitted)"),
    new: bool = typer.Option(False, "--new", help="create a fresh workspace"),
    name: str = typer.Option("CLI Workspace", help="name for a newly created workspace"),
    profile: Optional[str] = typer.Option(None),
):
    """Bind the active profile to a remote workspace (and ensure a thread)."""
    config, prof = _load(profile)
    _require_complete(prof)

    with StrobesClient(prof) as client:
        if new:
            result = client.create_workspace(name=name)
            ws = result.get("workspace", {})
            prof.workspace_id = ws.get("id")
            setup = result.get("setupThread") or {}
            if setup.get("id"):
                prof.thread_id = setup["id"]
            console.print(f"[green]✔ created workspace[/green] {prof.workspace_id} ({name})")
        else:
            if not workspace_id:
                rows = client.list_workspaces()
                if not rows:
                    console.print("[yellow]no workspaces — use --new to create one.[/yellow]")
                    raise typer.Exit(1)
                table = Table("#", "id", "name", "status")
                for i, w in enumerate(rows):
                    table.add_row(str(i), w["id"], w.get("name", ""), w.get("status", ""))
                console.print(table)
                idx = typer.prompt("Select workspace #", type=int)
                workspace_id = rows[idx]["id"]
            prof.workspace_id = workspace_id
            console.print(f"[green]✔ bound workspace[/green] {workspace_id}")

        # Ensure a thread exists for this binding.
        if not prof.thread_id:
            thread = client.create_thread(agent_ids=["orchestrator"], title="CLI session")
            tid = (thread.get("thread") or {}).get("id")
            if tid:
                prof.thread_id = tid
                console.print(f"[green]✔ created thread[/green] {tid}")

    config.save()


# ── chat ─────────────────────────────────────────────────────────────────


@app.command()
def chat(
    thread: Optional[str] = typer.Option(None, "--thread", "-t", help="thread UUID to resume"),
    workspace: Optional[str] = typer.Option(None, "--workspace", "-w", help="workspace UUID"),
    new_thread: bool = typer.Option(False, "--new", help="start a fresh thread"),
    model: Optional[int] = typer.Option(None, "--model", "-m", help="LLM model id"),
    local: bool = typer.Option(True, "--local/--no-local", help="run tools on this machine (CLI_LOCAL)"),
    auto_approve: bool = typer.Option(False, "--auto-approve", help="auto-approve agent actions"),
    headless: bool = typer.Option(False, "--headless", help="headless local browser"),
    workdir: Optional[str] = typer.Option(None, help="sandbox working directory"),
    profile: Optional[str] = typer.Option(None),
):
    """Start an interactive agent chat session."""
    config, prof = _load(profile)
    _require_complete(prof)

    if workspace:
        prof.workspace_id = workspace
    thread_id = thread or (None if new_thread else prof.thread_id)

    if not thread_id:
        try:
            with StrobesClient(prof) as client:
                created = client.create_thread(agent_ids=["orchestrator"], title="CLI session")
                thread_id = (created.get("thread") or {}).get("id")
        except StrobesAPIError as exc:
            console.print(f"[red]could not create thread:[/red] {exc}")
            raise typer.Exit(1)
        if not thread_id:
            console.print("[red]backend did not return a thread id.[/red]")
            raise typer.Exit(1)
        prof.thread_id = thread_id

    config.save()

    from .repl import chat_loop

    try:
        asyncio.run(
            chat_loop(
                prof, config.current_profile, thread_id, console=console,
                local_mode=local, auto_approve=auto_approve, headless=headless,
                llm_model=model, workdir=workdir,
            )
        )
    except KeyboardInterrupt:
        console.print("\n[dim]bye.[/dim]")


# ── bridge ───────────────────────────────────────────────────────────────


@app.command()
def bridge(
    no_browser: bool = typer.Option(False, "--no-browser", help="shell bridge only"),
    headless: bool = typer.Option(False, "--headless", help="headless browser"),
    workdir: Optional[str] = typer.Option(None, help="sandbox working directory"),
    profile: Optional[str] = typer.Option(None),
):
    """Run the local shell + browser bridges so a remote agent uses this machine."""
    config, prof = _load(profile)
    _require_complete(prof)

    from .bridges.supervisor import run_bridges

    console.print("[cyan]starting local bridges — Ctrl-C to stop[/cyan]")
    try:
        asyncio.run(
            run_bridges(
                prof, config, with_browser=not no_browser, headless=headless,
                workdir=workdir, console=console,
            )
        )
    except KeyboardInterrupt:
        console.print("\n[dim]bridges stopped.[/dim]")


def main() -> None:
    app()


if __name__ == "__main__":
    main()
