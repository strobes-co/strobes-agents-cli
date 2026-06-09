"""Persistent configuration for the Strobes AI CLI.

Stores connection profiles (base URL, organization id, MasterKey token) plus
the currently-bound workspace/thread in a JSON file under the user's config
dir (``~/.config/strobes-ai/config.json`` on Linux,
``~/Library/Application Support/strobes-ai/config.json`` on macOS).

A profile is everything needed to talk to one Strobes deployment as one user:

    {
      "current_profile": "default",
      "profiles": {
        "default": {
          "base_url": "https://app.strobes.co",
          "org_id": "0c0e...uuid",
          "master_key": "deadbeef...40hex",
          "deployment": "saas",          # or "enterprise" — controls path prefix
          "workspace_id": "...",          # last bound workspace (optional)
          "thread_id": "...",             # last bound thread (optional)
          "shell_bridge_id": "...",       # registered local shell (optional)
          "browser_id": "...",            # stable local browser id (optional)
          "verify_tls": true
        }
      }
    }

The MasterKey is a secret; the config file is written with 0600 permissions.
"""

from __future__ import annotations

import json
import os
import stat
import uuid
from dataclasses import dataclass, field, asdict
from pathlib import Path
from typing import Any, Optional

from platformdirs import user_config_dir

APP_NAME = "strobes-ai"
ENV_PREFIX = "STROBES_AI_"


def config_dir() -> Path:
    """Resolve (and create) the config directory."""
    override = os.environ.get(f"{ENV_PREFIX}HOME")
    base = Path(override) if override else Path(user_config_dir(APP_NAME, appauthor=False))
    base.mkdir(parents=True, exist_ok=True)
    return base


def config_path() -> Path:
    return config_dir() / "config.json"


@dataclass
class Profile:
    """One Strobes deployment binding."""

    base_url: str = ""
    org_id: str = ""
    master_key: str = ""
    deployment: str = "saas"  # "saas" | "enterprise"
    workspace_id: Optional[str] = None
    thread_id: Optional[str] = None
    shell_bridge_id: Optional[str] = None
    # Stable per-machine browser id so the same local browser re-registers
    # under one identity across runs.
    browser_id: Optional[str] = None
    verify_tls: bool = True

    def is_complete(self) -> bool:
        return bool(self.base_url and self.org_id and self.master_key)

    @property
    def api_prefix(self) -> str:
        """REST/GraphQL path prefix based on deployment mode.

        SaaS/MSSP mounts under ``/v1/``; enterprise under ``/api/v1/``.
        (Matches strobes/urls.py.)
        """
        return "/api/v1" if self.deployment == "enterprise" else "/v1"

    @property
    def graphql_path(self) -> str:
        return "/api/graphql/" if self.deployment == "enterprise" else "/v1/graphql/"


@dataclass
class Config:
    current_profile: str = "default"
    profiles: dict[str, Profile] = field(default_factory=dict)

    # ---- load / save ---------------------------------------------------

    @classmethod
    def load(cls) -> "Config":
        path = config_path()
        if not path.exists():
            return cls(profiles={"default": Profile()})
        try:
            raw = json.loads(path.read_text())
        except (json.JSONDecodeError, OSError):
            return cls(profiles={"default": Profile()})
        profiles = {
            name: Profile(**{k: v for k, v in p.items() if k in Profile.__dataclass_fields__})
            for name, p in raw.get("profiles", {}).items()
        }
        if not profiles:
            profiles = {"default": Profile()}
        return cls(
            current_profile=raw.get("current_profile", next(iter(profiles))),
            profiles=profiles,
        )

    def save(self) -> None:
        path = config_path()
        data = {
            "current_profile": self.current_profile,
            "profiles": {name: asdict(p) for name, p in self.profiles.items()},
        }
        tmp = path.with_suffix(".json.tmp")
        tmp.write_text(json.dumps(data, indent=2))
        os.replace(tmp, path)
        try:
            os.chmod(path, stat.S_IRUSR | stat.S_IWUSR)  # 0600
        except OSError:
            pass

    # ---- profile access ------------------------------------------------

    def profile(self, name: Optional[str] = None) -> Profile:
        name = name or self.current_profile
        if name not in self.profiles:
            self.profiles[name] = Profile()
        return self.profiles[name]

    def current(self) -> Profile:
        """Active profile, with environment-variable overrides applied.

        Env overrides (useful for CI / ephemeral runs):
          STROBES_AI_BASE_URL, STROBES_AI_ORG_ID, STROBES_AI_MASTER_KEY,
          STROBES_AI_DEPLOYMENT
        """
        p = self.profile()
        env = os.environ
        if env.get(f"{ENV_PREFIX}BASE_URL"):
            p.base_url = env[f"{ENV_PREFIX}BASE_URL"]
        if env.get(f"{ENV_PREFIX}ORG_ID"):
            p.org_id = env[f"{ENV_PREFIX}ORG_ID"]
        if env.get(f"{ENV_PREFIX}MASTER_KEY"):
            p.master_key = env[f"{ENV_PREFIX}MASTER_KEY"]
        if env.get(f"{ENV_PREFIX}DEPLOYMENT"):
            p.deployment = env[f"{ENV_PREFIX}DEPLOYMENT"]
        if not p.browser_id:
            # Persist a stable browser id the first time we touch the profile.
            p.browser_id = f"strobes-cli-{uuid.uuid4().hex[:12]}"
        return p

    def use(self, name: str) -> None:
        if name not in self.profiles:
            self.profiles[name] = Profile()
        self.current_profile = name


def redact(secret: str, keep: int = 4) -> str:
    if not secret:
        return "(unset)"
    if len(secret) <= keep:
        return "*" * len(secret)
    return secret[:keep] + "…" + secret[-keep:]
