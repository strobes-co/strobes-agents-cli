"""Offline tests for local execution, config, URL building, and the
pulse / shell-bridge wire protocols (mock WebSocket servers — no backend)."""

from __future__ import annotations

import asyncio
import json
import tempfile

import pytest
import websockets
from rich.console import Console

from strobes_ai.api import ws_base, ws_url
from strobes_ai.bridges.shell_bridge import ShellBridgeDaemon
from strobes_ai.config import Config, Profile
from strobes_ai.local.dispatch import LocalToolRouter
from strobes_ai.local.shell import LocalShell
from strobes_ai.render import EventRenderer
from strobes_ai.ws.pulse import AutoInteractor, PulseClient


def test_local_shell_exec_and_files():
    sh = LocalShell(workdir=tempfile.mkdtemp())
    r = sh.execute("echo hello")
    assert r["success"] and "hello" in r["stdout"]
    rc = sh.execute_code("print(2**10)", "python")
    assert "1024" in rc["stdout"]
    sh.write_file("note.txt", "hi")
    assert sh.read_file("note.txt")["content"] == "hi"
    assert any(f["name"] == "note.txt" for f in sh.list_files(".")["files"])


@pytest.mark.asyncio
async def test_local_router_dispatch():
    router = LocalToolRouter()
    router.shell = LocalShell(workdir=tempfile.mkdtemp())
    out = await router.execute("execute_command", {"command": "echo from-router"})
    assert "from-router" in out["output"] and out["exit_code"] == 0
    code = await router.execute("execute_code", {"code": "print(6*7)", "language": "python"})
    assert "42" in code["output"]
    meta = await router.execute("workspace_get_meta", {})
    assert "working_directory" in meta["output"]
    bad = await router.execute("nope", {})
    assert bad["error"]
    await router.close()


def test_config_round_trip(monkeypatch):
    monkeypatch.setenv("STROBES_AI_HOME", tempfile.mkdtemp())
    cfg = Config.load()
    p = cfg.profile("default")
    p.base_url, p.org_id, p.master_key, p.deployment = "https://app.strobes.co", "o", "k" * 40, "saas"
    cfg.save()
    again = Config.load().current()
    assert again.base_url == "https://app.strobes.co"
    assert again.api_prefix == "/v1" and again.graphql_path == "/v1/graphql/"
    assert again.browser_id


def test_url_building():
    p = Profile(base_url="https://app.strobes.co", org_id="org", master_key="k" * 40,
                deployment="saas", browser_id="b")
    u = ws_url(p, "/ws/org/pulse/T/")
    assert u == "wss://app.strobes.co/ws/org/pulse/T/?api_key=" + "k" * 40
    ent = Profile(base_url="http://localhost:8000", org_id="o", master_key="k", deployment="enterprise")
    assert ws_base(ent) == "ws://localhost:8000"
    assert ent.api_prefix == "/api/v1"


def test_renderer_smoke():
    rnd = EventRenderer(console=Console())
    for ev in [
        {"type": "system", "data": {"type": "run.started", "selectedAgents": ["o"]}},
        {"type": "token", "content": "hi", "agentName": "O"},
        {"type": "tool", "data": {"status": "start", "toolName": "x", "arguments": {}}},
        {"type": "tool", "data": {"status": "output", "toolName": "x", "result": "ok", "durationMs": 5}},
        {"type": "approval", "data": {"status": "requested", "approvalId": "a", "preview": "p"}},
        {"type": "system", "data": {"type": "run.completed", "metrics": {"total_tokens": 1}}},
    ]:
        rnd.handle(ev)
    rnd.close_stream()


@pytest.mark.asyncio
async def test_pulse_client_local_execute_and_approval():
    captured: dict = {}

    async def server(ws):
        captured["send"] = json.loads(await ws.recv())
        await ws.send(json.dumps({"type": "message_sent", "run_id": "r", "message_id": "m"}))
        await ws.send(json.dumps({"type": "pulse_event", "data": {
            "type": "tool", "data": {"status": "local_execute", "toolName": "execute_command",
                                     "requestId": "q", "input": {"command": "echo OK"}}}}))
        captured["result"] = json.loads(await ws.recv())
        await ws.send(json.dumps({"type": "pulse_event", "data": {
            "type": "approval", "data": {"status": "requested", "approvalId": "a", "preview": "x"}}}))
        captured["approval"] = json.loads(await ws.recv())
        await ws.send(json.dumps({"type": "pulse_event", "data": {
            "type": "system", "data": {"type": "run.completed", "metrics": {}}}}))
        await asyncio.sleep(0.1)

    async with websockets.serve(server, "127.0.0.1", 8787):
        p = Profile(base_url="http://127.0.0.1:8787", org_id="org", master_key="k" * 40,
                    deployment="enterprise", workspace_id="w", browser_id="b")
        client = PulseClient(p, "t", EventRenderer(console=Console()),
                             interactor=AutoInteractor(auto_approve=True), local_mode=True)
        await client.connect()
        await client.send_user_message("go")
        await asyncio.wait_for(client.wait_idle(), timeout=10)
        await client.close()

    assert captured["send"]["context"]["client_type"] == "cli"
    assert captured["result"]["type"] == "tool.local_result"
    assert "OK" in captured["result"]["payload"]["output"]
    assert captured["approval"]["decision"] == "approved"


@pytest.mark.asyncio
async def test_shell_bridge_protocol():
    captured: dict = {}

    async def server(ws):
        captured["identify"] = json.loads(await ws.recv())
        await ws.send(json.dumps({"type": "identify_ack", "data": {"bridge_id": "b9"}}))
        await ws.send(json.dumps({"type": "command", "command": "shell_execute",
                                  "params": {"command": "echo BRIDGE_OK"}, "request_id": "r1"}))
        captured["resp"] = json.loads(await ws.recv())

    async with websockets.serve(server, "127.0.0.1", 8786):
        p = Profile(base_url="http://127.0.0.1:8786", org_id="org", master_key="k" * 40,
                    deployment="enterprise", workspace_id="w", browser_id="b")
        d = ShellBridgeDaemon(p, bridge_id="b1", workdir=tempfile.mkdtemp(), console=Console())
        task = asyncio.create_task(d.run())
        await asyncio.sleep(0.8)
        d.stop()
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass

    assert captured["identify"]["type"] == "identify"
    assert "BRIDGE_OK" in captured["resp"]["data"]["stdout"]
