"""Local shell / code / file execution — the sandbox half of the bridge.

Implements the command set the Strobes shell-bridge daemon must support
(see strobes/agents/tools/utils/unified_session_manager.py):

  shell_execute        {command, timeout}        -> {success, stdout, stderr, exit_code, duration_ms, error}
  shell_execute_code   {language, code, timeout} -> {success, stdout, stderr, exit_code, duration_ms, error}
  file_write           {path, content}           -> {success, error}
  file_read            {path}                     -> {success, content, error}
  file_list            {directory}               -> {success, files[], error}

All execution happens in a persistent working directory (the "sandbox root")
so the agent sees a stable filesystem across calls, exactly like a real shell.
"""

from __future__ import annotations

import os
import shlex
import subprocess
import tempfile
import time
from datetime import datetime, timezone
from pathlib import Path
from typing import Any

# Map a logical language to (interpreter argv prefix, source file suffix).
_LANGUAGES: dict[str, tuple[list[str], str]] = {
    "python": (["python3"], ".py"),
    "python3": (["python3"], ".py"),
    "py": (["python3"], ".py"),
    "bash": (["bash"], ".sh"),
    "sh": (["sh"], ".sh"),
    "shell": (["bash"], ".sh"),
    "javascript": (["node"], ".js"),
    "js": (["node"], ".js"),
    "node": (["node"], ".js"),
    "typescript": (["npx", "--yes", "tsx"], ".ts"),
    "ts": (["npx", "--yes", "tsx"], ".ts"),
    "ruby": (["ruby"], ".rb"),
    "php": (["php"], ".php"),
    "go": (["go", "run"], ".go"),
}


class LocalShell:
    """A persistent local shell sandbox rooted at a working directory."""

    def __init__(self, workdir: str | None = None, env: dict[str, str] | None = None):
        if workdir:
            self.workdir = Path(workdir).expanduser().resolve()
        else:
            self.workdir = Path(
                os.environ.get("STROBES_AI_SANDBOX")
                or (Path.home() / ".strobes-ai" / "sandbox")
            ).resolve()
        self.workdir.mkdir(parents=True, exist_ok=True)
        self.base_env = {**os.environ, **(env or {})}

    # ---- command execution ---------------------------------------------

    def execute(self, command: str, timeout: float = 60.0) -> dict[str, Any]:
        """Run a shell command string in the sandbox via the user's shell."""
        return self._run(["/bin/bash", "-lc", command], timeout)

    def execute_code(
        self, code: str, language: str = "python", timeout: float = 120.0
    ) -> dict[str, Any]:
        """Write ``code`` to a temp file and run it with the right interpreter."""
        lang = (language or "python").lower().strip()
        spec = _LANGUAGES.get(lang)
        if not spec:
            return self._result(
                success=False,
                error=f"unsupported language: {language!r}",
                exit_code=127,
            )
        argv_prefix, suffix = spec
        fd, tmp_path = tempfile.mkstemp(suffix=suffix, dir=str(self.workdir))
        try:
            with os.fdopen(fd, "w") as fh:
                fh.write(code)
            return self._run([*argv_prefix, tmp_path], timeout)
        finally:
            try:
                os.unlink(tmp_path)
            except OSError:
                pass

    def _run(self, argv: list[str], timeout: float) -> dict[str, Any]:
        start = time.monotonic()
        try:
            proc = subprocess.run(
                argv,
                cwd=str(self.workdir),
                env=self.base_env,
                capture_output=True,
                text=True,
                timeout=max(1.0, float(timeout or 60.0)),
            )
            return self._result(
                success=proc.returncode == 0,
                stdout=proc.stdout,
                stderr=proc.stderr,
                exit_code=proc.returncode,
                duration_ms=int((time.monotonic() - start) * 1000),
            )
        except subprocess.TimeoutExpired as exc:
            return self._result(
                success=False,
                stdout=exc.stdout.decode() if isinstance(exc.stdout, bytes) else (exc.stdout or ""),
                stderr=(exc.stderr.decode() if isinstance(exc.stderr, bytes) else (exc.stderr or "")),
                exit_code=124,
                error=f"command timed out after {timeout}s",
                duration_ms=int((time.monotonic() - start) * 1000),
            )
        except FileNotFoundError as exc:
            return self._result(
                success=False,
                error=f"interpreter not found: {exc}",
                exit_code=127,
                duration_ms=int((time.monotonic() - start) * 1000),
            )
        except Exception as exc:  # noqa: BLE001 — daemon must never crash on a command
            return self._result(
                success=False,
                error=str(exc),
                exit_code=1,
                duration_ms=int((time.monotonic() - start) * 1000),
            )

    @staticmethod
    def _result(**kwargs: Any) -> dict[str, Any]:
        base = {
            "success": False,
            "stdout": "",
            "stderr": "",
            "exit_code": None,
            "duration_ms": 0,
            "error": None,
        }
        base.update(kwargs)
        return base

    # ---- file operations -----------------------------------------------

    def _resolve(self, path: str) -> Path:
        p = Path(path)
        return p if p.is_absolute() else (self.workdir / p)

    def write_file(self, path: str, content: str) -> dict[str, Any]:
        try:
            target = self._resolve(path)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(content)
            return {"success": True, "error": None}
        except Exception as exc:  # noqa: BLE001
            return {"success": False, "error": str(exc)}

    def read_file(self, path: str) -> dict[str, Any]:
        try:
            target = self._resolve(path)
            return {"success": True, "content": target.read_text(), "error": None}
        except Exception as exc:  # noqa: BLE001
            return {"success": False, "content": "", "error": str(exc)}

    def list_files(self, directory: str = ".") -> dict[str, Any]:
        try:
            target = self._resolve(directory)
            files = []
            for entry in sorted(target.iterdir()):
                try:
                    st = entry.stat()
                    files.append(
                        {
                            "name": entry.name,
                            "type": "directory" if entry.is_dir() else "file",
                            "size": st.st_size,
                            "modified": datetime.fromtimestamp(
                                st.st_mtime, tz=timezone.utc
                            ).isoformat(),
                        }
                    )
                except OSError:
                    continue
            return {"success": True, "files": files, "error": None}
        except Exception as exc:  # noqa: BLE001
            return {"success": False, "files": [], "error": str(exc)}

    # ---- metadata (for CLI_LOCAL workspace_get_meta) -------------------

    def meta(self) -> dict[str, Any]:
        try:
            entries = list(self.workdir.iterdir())
        except OSError:
            entries = []
        return {
            "working_directory": str(self.workdir),
            "hostname": os.uname().nodename if hasattr(os, "uname") else "",
            "platform": __import__("platform").platform(),
            "shell": os.environ.get("SHELL", "/bin/bash"),
            "file_count": len(entries),
            "user": os.environ.get("USER", ""),
        }
