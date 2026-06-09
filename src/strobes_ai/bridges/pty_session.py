"""Interactive PTY sessions for the shell bridge.

The backend's TerminalConsumer sends raw PTY control messages that the
ShellBridgeConsumer forwards verbatim:

  pty_open   {session_id, cols, rows}
  pty_input  {session_id, data}
  pty_resize {session_id, cols, rows}
  pty_close  {session_id}

The daemon streams terminal output back as:

  pty_output {session_id, data}
  pty_closed {session_id}

This module manages those sessions on POSIX using the stdlib ``pty`` module.
"""

from __future__ import annotations

import asyncio
import fcntl
import os
import signal
import struct
import termios
from typing import Awaitable, Callable

OutputCb = Callable[[str, str], Awaitable[None]]  # (session_id, data)
ClosedCb = Callable[[str], Awaitable[None]]       # (session_id)


class PtySession:
    def __init__(
        self,
        session_id: str,
        cols: int,
        rows: int,
        on_output: OutputCb,
        on_closed: ClosedCb,
        shell: str | None = None,
    ):
        self.session_id = session_id
        self.on_output = on_output
        self.on_closed = on_closed
        self.shell = shell or os.environ.get("SHELL", "/bin/bash")
        self.pid: int | None = None
        self.master_fd: int | None = None
        self._loop = asyncio.get_event_loop()
        self._open(cols, rows)

    def _open(self, cols: int, rows: int) -> None:
        import pty

        pid, master_fd = pty.fork()
        if pid == 0:  # child
            try:
                os.execvp(self.shell, [self.shell, "-i"])
            except Exception:  # noqa: BLE001
                os._exit(127)
        # parent
        self.pid = pid
        self.master_fd = master_fd
        self.resize(cols, rows)
        os.set_blocking(master_fd, False)
        self._loop.add_reader(master_fd, self._on_readable)

    def _on_readable(self) -> None:
        if self.master_fd is None:
            return
        try:
            data = os.read(self.master_fd, 65536)
        except (OSError, BlockingIOError):
            self._teardown()
            return
        if not data:
            self._teardown()
            return
        asyncio.ensure_future(
            self.on_output(self.session_id, data.decode(errors="replace"))
        )

    def write(self, data: str) -> None:
        if self.master_fd is not None:
            try:
                os.write(self.master_fd, data.encode())
            except OSError:
                self._teardown()

    def resize(self, cols: int, rows: int) -> None:
        if self.master_fd is None:
            return
        try:
            winsize = struct.pack("HHHH", rows, cols, 0, 0)
            fcntl.ioctl(self.master_fd, termios.TIOCSWINSZ, winsize)
        except OSError:
            pass

    def close(self) -> None:
        self._teardown()

    def _teardown(self) -> None:
        fd = self.master_fd
        if fd is not None:
            try:
                self._loop.remove_reader(fd)
            except Exception:  # noqa: BLE001
                pass
            try:
                os.close(fd)
            except OSError:
                pass
            self.master_fd = None
        if self.pid:
            try:
                os.kill(self.pid, signal.SIGTERM)
            except OSError:
                pass
            self.pid = None
        asyncio.ensure_future(self.on_closed(self.session_id))
