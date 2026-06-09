"""HTTP (REST + GraphQL) client for the Strobes backend.

Authentication uses a MasterKey via ``Authorization: token <key>`` (matches
``strobes/app/authentication.py``). WebSocket connections authenticate with the
same key passed as the ``?api_key=`` query parameter (matches
``strobes/channels_middleware.py``).

Endpoints used:
  GET  {prefix}/organizations/{org}/cli/workspaces/      list workspaces
  GET  {prefix}/organizations/{org}/cli/threads/         list threads
  POST {prefix}/organizations/{org}/shells/              register a (bridge) shell
  POST {prefix}/organizations/{org}/pulse/threads/{t}/attach-shell/
  POST {graphql_path}                                    createWorkspace / createThread
"""

from __future__ import annotations

from typing import Any, Optional
from urllib.parse import urlparse, urlunparse

import httpx

from .config import Profile


class StrobesAPIError(RuntimeError):
    def __init__(self, message: str, status: int | None = None, payload: Any = None):
        super().__init__(message)
        self.status = status
        self.payload = payload


def _normalize_base(base_url: str) -> str:
    base = base_url.strip().rstrip("/")
    if not base:
        raise StrobesAPIError("base_url is empty — run `strobes-ai login` first.")
    if "://" not in base:
        base = "https://" + base
    return base


def http_base(profile: Profile) -> str:
    return _normalize_base(profile.base_url)


def ws_base(profile: Profile) -> str:
    """Derive the ws:// or wss:// origin from the configured base URL."""
    parsed = urlparse(_normalize_base(profile.base_url))
    scheme = "wss" if parsed.scheme == "https" else "ws"
    return urlunparse((scheme, parsed.netloc, "", "", "", "")).rstrip("/")


def ws_url(profile: Profile, path: str, **query: str) -> str:
    """Build a WS URL with the api_key and any extra query params."""
    from urllib.parse import urlencode

    q = {"api_key": profile.master_key}
    q.update({k: v for k, v in query.items() if v is not None})
    base = ws_base(profile).rstrip("/")
    normalized = "/" + path.lstrip("/")
    return f"{base}{normalized}?{urlencode(q)}"


class StrobesClient:
    """Thin synchronous REST/GraphQL client bound to one profile."""

    def __init__(self, profile: Profile, timeout: float = 30.0):
        self.profile = profile
        self._client = httpx.Client(
            base_url=http_base(profile),
            headers={
                "Authorization": f"token {profile.master_key}",
                "Accept": "application/json",
                "User-Agent": "strobes-ai-cli/0.1",
            },
            verify=profile.verify_tls,
            timeout=timeout,
            follow_redirects=True,
        )

    def __enter__(self) -> "StrobesClient":
        return self

    def __exit__(self, *exc: Any) -> None:
        self.close()

    def close(self) -> None:
        self._client.close()

    # ---- low level -----------------------------------------------------

    def _request(self, method: str, path: str, **kwargs: Any) -> Any:
        try:
            resp = self._client.request(method, path, **kwargs)
        except httpx.RequestError as exc:
            raise StrobesAPIError(f"network error talking to Strobes: {exc}") from exc
        if resp.status_code >= 400:
            detail: Any
            try:
                detail = resp.json()
            except Exception:
                detail = resp.text[:500]
            raise StrobesAPIError(
                f"{method} {path} → HTTP {resp.status_code}: {detail}",
                status=resp.status_code,
                payload=detail,
            )
        if resp.status_code == 204 or not resp.content:
            return None
        try:
            return resp.json()
        except Exception:
            return resp.text

    @property
    def _org(self) -> str:
        return self.profile.org_id

    @property
    def _p(self) -> str:
        return self.profile.api_prefix

    # ---- REST: discovery -----------------------------------------------

    def list_workspaces(self) -> list[dict]:
        return self._request("GET", f"{self._p}/organizations/{self._org}/cli/workspaces/") or []

    def list_threads(self) -> list[dict]:
        return self._request("GET", f"{self._p}/organizations/{self._org}/cli/threads/") or []

    # ---- REST: shells (bridge registration) ----------------------------

    def register_bridge_shell(self, name: str, description: str = "") -> dict:
        """Create a ``shell_type=bridge`` Shell row and return it (with bridge_id)."""
        body = {
            "shell_type": "bridge",
            "name": name,
            "description": description or f"Local shell bridge ({name})",
            "is_active": True,
        }
        return self._request(
            "POST", f"{self._p}/organizations/{self._org}/shells/", json=body
        )

    def list_shells(self) -> list[dict]:
        data = self._request("GET", f"{self._p}/organizations/{self._org}/shells/")
        if isinstance(data, dict) and "results" in data:
            return data["results"]
        return data or []

    def attach_shell_to_workspace(self, workspace_id: str, shell_id: str) -> Any:
        return self._request(
            "POST",
            f"{self._p}/organizations/{self._org}/workspaces/{workspace_id}/attach-shell/",
            json={"shell_id": shell_id},
        )

    def attach_shell_to_thread(self, thread_id: str, shell_id: str) -> Any:
        return self._request(
            "POST",
            f"{self._p}/organizations/{self._org}/pulse/threads/{thread_id}/attach-shell/",
            json={"shell_id": shell_id},
        )

    # ---- GraphQL --------------------------------------------------------

    def graphql(self, query: str, variables: dict | None = None) -> dict:
        resp = self._request(
            "POST",
            self.profile.graphql_path,
            json={"query": query, "variables": variables or {}},
        )
        if isinstance(resp, dict) and resp.get("errors"):
            raise StrobesAPIError(
                f"GraphQL error: {resp['errors']}", payload=resp["errors"]
            )
        return (resp or {}).get("data", {}) if isinstance(resp, dict) else {}

    def create_workspace(
        self,
        name: str = "New Workspace",
        engagement_id: Optional[str] = None,
        settings: Optional[dict] = None,
        shell_id: Optional[str] = None,
    ) -> dict:
        query = """
        mutation CreateWorkspace($organizationId: UUID!, $name: String,
                                 $engagementId: UUID, $settings: GenericScalar,
                                 $shellId: UUID) {
          createWorkspace(organizationId: $organizationId, name: $name,
                          engagementId: $engagementId, settings: $settings,
                          shellId: $shellId) {
            workspace { id name description status engagementId createdAt }
            setupThread { id title status }
          }
        }
        """
        variables = {
            "organizationId": self._org,
            "name": name,
            "engagementId": engagement_id,
            "settings": settings,
            "shellId": shell_id,
        }
        data = self.graphql(query, variables)
        return data.get("createWorkspace", {})

    def create_thread(
        self,
        agent_ids: list[str],
        title: Optional[str] = None,
        context: Optional[dict] = None,
        mode: str = "chat",
        smart_mode: bool = True,
        shell_id: Optional[str] = None,
        safety_mode: str = "safe",
    ) -> dict:
        query = """
        mutation CreateThread($organizationId: UUID!, $agentIds: [String!]!,
                              $title: String, $context: GenericScalar, $mode: String,
                              $smartMode: Boolean, $shellId: UUID, $safetyMode: String) {
          createThread(organizationId: $organizationId, agentIds: $agentIds,
                       title: $title, context: $context, mode: $mode,
                       smartMode: $smartMode, shellId: $shellId, safetyMode: $safetyMode) {
            thread { id title status }
            requiredCredentials { key displayName }
          }
        }
        """
        variables = {
            "organizationId": self._org,
            "agentIds": agent_ids or ["orchestrator"],
            "title": title,
            "context": context,
            "mode": mode,
            "smartMode": smart_mode,
            "shellId": shell_id,
            "safetyMode": safety_mode,
        }
        data = self.graphql(query, variables)
        return data.get("createThread", {})

    # ---- connectivity check --------------------------------------------

    def ping(self) -> bool:
        """Cheap auth check — listing workspaces validates the MasterKey + org."""
        self.list_workspaces()
        return True
