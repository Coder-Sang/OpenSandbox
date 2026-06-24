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
"""Synchronous PTY service adapter."""

from typing import Any
from urllib.parse import urlencode

import httpx

from opensandbox.adapters.converter.response_handler import extract_request_id
from opensandbox.config.connection_sync import ConnectionConfigSync
from opensandbox.exceptions import SandboxApiException
from opensandbox.models.pty import (
    CreatePTYSessionResponse,
    PTYSessionStatus,
)
from opensandbox.models.sandboxes import SandboxEndpoint
from opensandbox.sync.services.pty import PTYSync


class PTYAdapterSync(PTYSync):
    """Sync adapter for execd PTY REST and WebSocket endpoints."""

    def __init__(
        self,
        connection_config: ConnectionConfigSync,
        execd_endpoint: SandboxEndpoint,
    ) -> None:
        self.connection_config = connection_config
        self.execd_endpoint = execd_endpoint

        self._base_url = f"{self.connection_config.protocol}://{self.execd_endpoint.endpoint}"
        self._timeout_seconds = self.connection_config.request_timeout.total_seconds()
        timeout = httpx.Timeout(self._timeout_seconds)
        self._headers = {
            "User-Agent": self.connection_config.user_agent,
            **self.connection_config.headers,
            **self.execd_endpoint.headers,
        }
        self._httpx_client = httpx.Client(
            base_url=self._base_url,
            headers=self._headers,
            timeout=timeout,
            transport=self.connection_config.transport,
            follow_redirects=self.connection_config.follow_redirects,
        )

    def create_session(
        self,
        *,
        cwd: str | None = None,
        command: str | None = None,
    ) -> str:
        body = {
            k: v for k, v in {"cwd": cwd, "command": command}.items() if v is not None
        }
        response = self._httpx_client.post("/pty", json=body)
        self._raise_for_status(response, "Create PTY session")
        parsed = CreatePTYSessionResponse.model_validate(response.json())
        return parsed.session_id

    def get_session_status(self, session_id: str) -> PTYSessionStatus:
        response = self._httpx_client.get(f"/pty/{session_id}")
        self._raise_for_status(response, f"Get PTY session {session_id}")
        return PTYSessionStatus.model_validate(response.json())

    def delete_session(self, session_id: str) -> None:
        response = self._httpx_client.delete(f"/pty/{session_id}")
        self._raise_for_status(response, f"Delete PTY session {session_id}")

    def connect_websocket(
        self,
        session_id: str,
        *,
        pty: bool = True,
        since: int = 0,
        takeover: bool = False,
    ) -> Any:
        url = self._build_ws_url(session_id, pty=pty, since=since, takeover=takeover)
        return self._connect_websocket(url)

    def _build_ws_url(
        self,
        session_id: str,
        *,
        pty: bool,
        since: int,
        takeover: bool,
    ) -> str:
        scheme = "wss" if self.connection_config.protocol == "https" else "ws"
        url = f"{scheme}://{self.execd_endpoint.endpoint}/pty/{session_id}/ws"
        query: dict[str, str] = {}
        if not pty:
            query["pty"] = "0"
        if since:
            query["since"] = str(since)
        if takeover:
            query["takeover"] = "1"
        if query:
            url = f"{url}?{urlencode(query)}"
        return url

    def _connect_websocket(self, url: str) -> Any:
        from websockets.sync.client import connect

        return connect(
            url,
            additional_headers=self._headers,
            open_timeout=self._timeout_seconds,
        )

    def _raise_for_status(self, response: httpx.Response, operation: str) -> None:
        if response.status_code < 400:
            return
        raise SandboxApiException(
            message=f"{operation} failed. Status code: {response.status_code}",
            status_code=response.status_code,
            request_id=extract_request_id(response.headers),
        )
