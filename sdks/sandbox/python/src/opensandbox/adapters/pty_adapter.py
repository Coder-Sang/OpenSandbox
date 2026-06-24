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
"""PTY service adapter."""

import logging
from typing import Any
from urllib.parse import urlencode

import httpx

from opensandbox.adapters.converter.response_handler import extract_request_id
from opensandbox.config import ConnectionConfig
from opensandbox.exceptions import SandboxApiException
from opensandbox.models.pty import (
    CreatePTYSessionResponse,
    PTYSessionStatus,
)
from opensandbox.models.sandboxes import SandboxEndpoint
from opensandbox.services.pty import PTY

logger = logging.getLogger(__name__)


class PTYAdapter(PTY):
    """Adapter for execd PTY REST and WebSocket endpoints."""

    def __init__(
        self,
        connection_config: ConnectionConfig,
        execd_endpoint: SandboxEndpoint,
    ) -> None:
        self.connection_config = connection_config
        self.execd_endpoint = execd_endpoint

        protocol = self.connection_config.protocol
        self._base_url = f"{protocol}://{self.execd_endpoint.endpoint}"
        self._timeout_seconds = self.connection_config.request_timeout.total_seconds()
        timeout = httpx.Timeout(self._timeout_seconds)
        self._headers = {
            "User-Agent": self.connection_config.user_agent,
            **self.connection_config.headers,
            **self.execd_endpoint.headers,
        }
        self._httpx_client = httpx.AsyncClient(
            base_url=self._base_url,
            headers=self._headers,
            timeout=timeout,
            transport=self.connection_config.transport,
            follow_redirects=self.connection_config.follow_redirects,
        )

    async def create_session(
        self,
        *,
        cwd: str | None = None,
        command: str | None = None,
    ) -> str:
        body = {
            k: v for k, v in {"cwd": cwd, "command": command}.items() if v is not None
        }
        response = await self._httpx_client.post("/pty", json=body)
        await self._raise_for_status(response, "Create PTY session")
        parsed = CreatePTYSessionResponse.model_validate(response.json())
        return parsed.session_id

    async def get_session_status(self, session_id: str) -> PTYSessionStatus:
        response = await self._httpx_client.get(f"/pty/{session_id}")
        await self._raise_for_status(response, f"Get PTY session {session_id}")
        return PTYSessionStatus.model_validate(response.json())

    async def delete_session(self, session_id: str) -> None:
        response = await self._httpx_client.delete(f"/pty/{session_id}")
        await self._raise_for_status(response, f"Delete PTY session {session_id}")

    async def connect_websocket(
        self,
        session_id: str,
        *,
        pty: bool = True,
        since: int = 0,
        takeover: bool = False,
    ) -> Any:
        url = self._build_ws_url(session_id, pty=pty, since=since, takeover=takeover)
        return await self._connect_websocket(url)

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

    async def _connect_websocket(self, url: str) -> Any:
        from websockets.asyncio.client import connect

        return await connect(
            url,
            additional_headers=self._headers,
            open_timeout=self._timeout_seconds,
        )

    async def _raise_for_status(
        self, response: httpx.Response, operation: str
    ) -> None:
        if response.status_code < 400:
            return
        await response.aread()
        raise SandboxApiException(
            message=f"{operation} failed. Status code: {response.status_code}",
            status_code=response.status_code,
            request_id=extract_request_id(response.headers),
        )
