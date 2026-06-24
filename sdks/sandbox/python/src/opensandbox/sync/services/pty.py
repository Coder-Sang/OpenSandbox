#
# Copyright 2026 Alibaba Group Holding Ltd.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     http://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
"""Synchronous PTY service interface."""

from typing import Any, Protocol

from opensandbox.models.pty import PTYSessionStatus


class PTYSync(Protocol):
    """Synchronous interactive PTY session operations."""

    def create_session(
        self,
        *,
        cwd: str | None = None,
        command: str | None = None,
    ) -> str:
        """Create a PTY session and return its session ID."""
        ...

    def get_session_status(self, session_id: str) -> PTYSessionStatus:
        """Get PTY session status."""
        ...

    def delete_session(self, session_id: str) -> None:
        """Delete a PTY session."""
        ...

    def connect_websocket(
        self,
        session_id: str,
        *,
        pty: bool = True,
        since: int = 0,
        takeover: bool = False,
    ) -> Any:
        """Open the PTY WebSocket and return the underlying websockets connection."""
        ...
