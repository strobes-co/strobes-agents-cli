"""Route ``tool.local_execute`` events to local executors.

The pulse client receives ``tool.local_execute`` events of the form::

    {status: "local_execute", toolName, requestId, input: {...}, timeout}

and must reply with ``tool.local_result`` / ``tool.local_error``. The result
payload is consumed by the cloud LocalProxyTool, which reads ``output``,
``exit_code``, and ``error`` (see strobes/.../local_proxy.py).

This router maps the proxied tool names to the shared local executors:
  execute_command, execute_code, workspace_get_meta, and the browser_* set.
"""

from __future__ import annotations

import asyncio
import json
from typing import Any, Optional

from .browser import BrowserUnavailable, LocalBrowser
from .shell import LocalShell

_BROWSER_TOOLS = {
    "browser_navigate",
    "browser_click",
    "browser_type",
    "browser_screenshot",
    "browser_snapshot",
    "browser_execute_script",
    "browser_get_cookies",
    "browser_scroll",
    "browser_init",
}


class LocalToolRouter:
    """Holds the persistent local shell + browser used for CLI_LOCAL tools."""

    def __init__(self, shell: Optional[LocalShell] = None, headless: bool = False):
        self.shell = shell or LocalShell()
        self.headless = headless
        self._browser: Optional[LocalBrowser] = None

    def _get_browser(self) -> LocalBrowser:
        if self._browser is None:
            self._browser = LocalBrowser(headless=self.headless)
        return self._browser

    async def close(self) -> None:
        if self._browser is not None:
            await self._browser.close()

    async def execute(self, tool_name: str, tool_input: dict[str, Any]) -> dict[str, Any]:
        """Execute one proxied tool locally; return {output, exit_code, error}."""
        tool_input = tool_input or {}
        try:
            if tool_name == "execute_command":
                return self._fmt_shell(
                    await asyncio.to_thread(
                        self.shell.execute, tool_input.get("command", ""),
                        tool_input.get("timeout", 60),
                    )
                )
            if tool_name == "execute_code":
                return self._fmt_shell(
                    await asyncio.to_thread(
                        self.shell.execute_code,
                        tool_input.get("code", ""),
                        tool_input.get("language", "python"),
                        tool_input.get("timeout", 120),
                    )
                )
            if tool_name == "workspace_get_meta":
                return {"output": json.dumps(self.shell.meta(), indent=2), "exit_code": 0}
            if tool_name in _BROWSER_TOOLS:
                return await self._browser_exec(tool_name, tool_input)
            return {
                "output": "",
                "error": f"unsupported local tool: {tool_name}",
                "error_type": "UnsupportedTool",
            }
        except BrowserUnavailable as exc:
            return {"output": "", "error": str(exc), "error_type": "BrowserUnavailable"}
        except Exception as exc:  # noqa: BLE001 — never break the WS loop
            return {"output": "", "error": str(exc), "error_type": type(exc).__name__}

    # ---- formatting ----------------------------------------------------

    @staticmethod
    def _fmt_shell(res: dict[str, Any]) -> dict[str, Any]:
        parts = []
        if res.get("stdout"):
            parts.append(res["stdout"].rstrip("\n"))
        if res.get("stderr"):
            parts.append(res["stderr"].rstrip("\n"))
        output = "\n".join(p for p in parts if p)
        out: dict[str, Any] = {"output": output, "exit_code": res.get("exit_code")}
        if res.get("error"):
            out["error"] = res["error"]
            out["error_type"] = "ExecutionError"
        return out

    async def _browser_exec(self, tool_name: str, tool_input: dict[str, Any]) -> dict[str, Any]:
        res = await self._get_browser().handle(tool_name, tool_input)
        if not res.get("success"):
            return {"output": "", "error": res.get("error", "browser command failed"),
                    "error_type": "BrowserError"}
        # Shape the human/agent-readable output per tool.
        if tool_name == "browser_navigate":
            return {"output": f"{res.get('title', '')} — {res.get('url', '')}".strip(" —"),
                    "exit_code": 0}
        if tool_name == "browser_snapshot":
            return {"output": res.get("snapshot", ""), "exit_code": 0}
        if tool_name == "browser_screenshot":
            return {"output": res.get("data_url", ""), "exit_code": 0}
        if tool_name == "browser_execute_script":
            return {"output": json.dumps(res.get("result"), default=str), "exit_code": 0}
        if tool_name == "browser_get_cookies":
            return {"output": json.dumps(res.get("cookies", []), default=str), "exit_code": 0}
        # click / type / scroll / init
        return {"output": json.dumps({k: v for k, v in res.items() if k != "success"}),
                "exit_code": 0}
