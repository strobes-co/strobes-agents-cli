"""Local browser automation via Playwright — the browser half of the bridge.

Implements the command set the Strobes browser-bridge expects
(see strobes/agents/tools/implementations/local_browser.py):

  browser_init           {name}                                  -> {success, tab_id}
  browser_navigate       {url, wait_for_selector?, timeout?}     -> {success, url, status_code}
  browser_snapshot       {selector?}                             -> {success, snapshot}
  browser_click          {selector, wait_after?, timeout?}       -> {success, element_found, clicked}
  browser_scroll         {direction, amount, selector?}          -> {success, scrolled, scroll_x, scroll_y}
  browser_screenshot     {selector?, full_page?}                 -> {success, data_url, width, height}
  browser_execute_script {script, timeout?}                      -> {success, result}
  browser_type           {selector, text, clear?}                -> {success, typed, value}
  browser_get_cookies    {}                                      -> {success, cookies}

Playwright is an optional dependency. Install with:
    pip install 'strobes-ai-cli[browser]' && playwright install chromium
"""

from __future__ import annotations

import asyncio
import base64
from typing import Any, Optional


class BrowserUnavailable(RuntimeError):
    pass


def _require_playwright():
    try:
        from playwright.async_api import async_playwright  # noqa: F401
    except ImportError as exc:  # pragma: no cover
        raise BrowserUnavailable(
            "Playwright is not installed. Run:\n"
            "    pip install 'strobes-ai-cli[browser]'\n"
            "    playwright install chromium"
        ) from exc
    from playwright.async_api import async_playwright

    return async_playwright


class LocalBrowser:
    """A single persistent Chromium page driven by Playwright.

    Methods mirror the bridge command set and always return a dict with a
    ``success`` key plus command-specific fields (and ``error`` on failure),
    so they never raise into the WebSocket loop.
    """

    def __init__(self, headless: bool = False):
        self.headless = headless
        self._pw = None
        self._browser = None
        self._context = None
        self._page = None
        self._lock = asyncio.Lock()

    async def _ensure_page(self):
        if self._page is not None:
            return
        async_playwright = _require_playwright()
        self._pw = await async_playwright().start()
        self._browser = await self._pw.chromium.launch(headless=self.headless)
        self._context = await self._browser.new_context()
        self._page = await self._context.new_page()

    async def close(self) -> None:
        try:
            if self._context:
                await self._context.close()
            if self._browser:
                await self._browser.close()
            if self._pw:
                await self._pw.stop()
        except Exception:  # noqa: BLE001
            pass
        finally:
            self._page = self._context = self._browser = self._pw = None

    # ---- command handlers ----------------------------------------------

    async def init(self, **_: Any) -> dict[str, Any]:
        try:
            await self._ensure_page()
            return {"success": True, "tab_id": 1}
        except Exception as exc:  # noqa: BLE001
            return {"success": False, "error": str(exc)}

    async def navigate(
        self, url: str, wait_for_selector: Optional[str] = None,
        timeout: int = 30000, **_: Any,
    ) -> dict[str, Any]:
        async with self._lock:
            try:
                await self._ensure_page()
                resp = await self._page.goto(url, timeout=_ms(timeout), wait_until="domcontentloaded")
                if wait_for_selector:
                    await self._page.wait_for_selector(wait_for_selector, timeout=_ms(timeout))
                return {
                    "success": True,
                    "url": self._page.url,
                    "title": await self._page.title(),
                    "status_code": resp.status if resp else None,
                }
            except Exception as exc:  # noqa: BLE001
                return {"success": False, "error": str(exc)}

    async def snapshot(self, selector: Optional[str] = None, **_: Any) -> dict[str, Any]:
        """Compact accessibility-ish tree of interactive elements."""
        async with self._lock:
            try:
                await self._ensure_page()
                js = """(sel) => {
                    const root = sel ? document.querySelector(sel) : document.body;
                    if (!root) return '';
                    const out = [];
                    const want = 'a,button,input,textarea,select,[role],[onclick],h1,h2,h3,label';
                    root.querySelectorAll(want).forEach((el, i) => {
                        if (i > 400) return;
                        const tag = el.tagName.toLowerCase();
                        const role = el.getAttribute('role') || '';
                        const text = (el.innerText || el.value || el.getAttribute('aria-label')
                                      || el.getAttribute('placeholder') || '').trim().slice(0, 80);
                        const id = el.id ? '#' + el.id : '';
                        const name = el.getAttribute('name');
                        const sel = id || (name ? `${tag}[name="${name}"]` : tag);
                        out.push(`${tag}${role ? '['+role+']' : ''} ${sel} :: ${text}`);
                    });
                    return out.join('\\n');
                }"""
                snap = await self._page.evaluate(js, selector)
                title = await self._page.title()
                header = f"# {title}\n# {self._page.url}\n"
                return {"success": True, "snapshot": header + snap}
            except Exception as exc:  # noqa: BLE001
                return {"success": False, "error": str(exc)}

    async def click(
        self, selector: str, wait_after: Optional[str] = None,
        timeout: int = 30000, **_: Any,
    ) -> dict[str, Any]:
        async with self._lock:
            try:
                await self._ensure_page()
                locator = self._page.locator(selector).first
                count = await locator.count()
                if count == 0:
                    return {"success": False, "element_found": False, "clicked": False,
                            "error": f"no element matches {selector!r}"}
                await locator.click(timeout=_ms(timeout))
                if wait_after:
                    await self._page.wait_for_selector(wait_after, timeout=_ms(timeout))
                return {"success": True, "element_found": True, "clicked": True}
            except Exception as exc:  # noqa: BLE001
                return {"success": False, "element_found": True, "clicked": False, "error": str(exc)}

    async def type(
        self, selector: str, text: str, clear: bool = True, **_: Any
    ) -> dict[str, Any]:
        async with self._lock:
            try:
                await self._ensure_page()
                locator = self._page.locator(selector).first
                if clear:
                    await locator.fill(text)
                else:
                    await locator.click()
                    await locator.type(text)
                value = await locator.input_value() if await locator.count() else text
                return {"success": True, "typed": True, "value": value}
            except Exception as exc:  # noqa: BLE001
                return {"success": False, "typed": False, "error": str(exc)}

    async def scroll(
        self, direction: str = "down", amount: int = 600,
        selector: Optional[str] = None, **_: Any,
    ) -> dict[str, Any]:
        async with self._lock:
            try:
                await self._ensure_page()
                dy = amount if direction in ("down", "bottom") else -amount
                if direction in ("top",):
                    await self._page.evaluate("window.scrollTo(0, 0)")
                elif direction in ("bottom",):
                    await self._page.evaluate("window.scrollTo(0, document.body.scrollHeight)")
                else:
                    await self._page.evaluate("(d) => window.scrollBy(0, d)", dy)
                pos = await self._page.evaluate("() => [window.scrollX, window.scrollY]")
                return {"success": True, "scrolled": True,
                        "scroll_x": pos[0], "scroll_y": pos[1]}
            except Exception as exc:  # noqa: BLE001
                return {"success": False, "scrolled": False, "error": str(exc)}

    async def screenshot(
        self, selector: Optional[str] = None, full_page: bool = False, **_: Any
    ) -> dict[str, Any]:
        async with self._lock:
            try:
                await self._ensure_page()
                if selector:
                    png = await self._page.locator(selector).first.screenshot()
                else:
                    png = await self._page.screenshot(full_page=full_page)
                vp = self._page.viewport_size or {}
                b64 = base64.b64encode(png).decode()
                return {
                    "success": True,
                    "data_url": f"data:image/png;base64,{b64}",
                    "width": vp.get("width"),
                    "height": vp.get("height"),
                }
            except Exception as exc:  # noqa: BLE001
                return {"success": False, "error": str(exc)}

    async def execute_script(self, script: str, timeout: int = 30000, **_: Any) -> dict[str, Any]:
        async with self._lock:
            try:
                await self._ensure_page()
                # Wrap so both expression and statement bodies work.
                result = await self._page.evaluate(f"() => {{ {script} }}")
                return {"success": True, "result": result}
            except Exception:  # noqa: BLE001 — retry as a bare expression
                try:
                    result = await self._page.evaluate(script)
                    return {"success": True, "result": result}
                except Exception as exc:  # noqa: BLE001
                    return {"success": False, "error": str(exc)}

    async def get_cookies(self, **_: Any) -> dict[str, Any]:
        async with self._lock:
            try:
                await self._ensure_page()
                cookies = await self._context.cookies()
                return {"success": True, "cookies": cookies}
            except Exception as exc:  # noqa: BLE001
                return {"success": False, "error": str(exc)}

    # ---- dispatch ------------------------------------------------------

    async def handle(self, command: str, params: dict[str, Any]) -> dict[str, Any]:
        """Route a ``browser_*`` command name to the matching handler."""
        handlers = {
            "browser_init": self.init,
            "browser_navigate": self.navigate,
            "browser_snapshot": self.snapshot,
            "browser_click": self.click,
            "browser_type": self.type,
            "browser_scroll": self.scroll,
            "browser_screenshot": self.screenshot,
            "browser_execute_script": self.execute_script,
            "browser_get_cookies": self.get_cookies,
        }
        handler = handlers.get(command)
        if not handler:
            return {"success": False, "error": f"unknown browser command: {command}"}
        # Drop the bridge's routing-only ``browser_name`` key before dispatch.
        kwargs = {k: v for k, v in (params or {}).items() if k != "browser_name"}
        return await handler(**kwargs)


def _ms(value: int | float | None) -> float:
    """Normalize a timeout to milliseconds. Values <1000 are treated as seconds."""
    if not value:
        return 30000.0
    return float(value) if value >= 1000 else float(value) * 1000.0
