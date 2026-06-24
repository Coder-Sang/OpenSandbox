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
import json

import httpx
import pytest

from opensandbox.adapters.pty_adapter import PTYAdapter
from opensandbox.config import ConnectionConfig
from opensandbox.config.connection_sync import ConnectionConfigSync
from opensandbox.models.pty import encode_pty_stdin
from opensandbox.models.sandboxes import SandboxEndpoint
from opensandbox.sandbox import Sandbox
from opensandbox.sync.adapters.pty_adapter import PTYAdapterSync
from opensandbox.sync.sandbox import SandboxSync


class _AsyncPTYTransport(httpx.AsyncBaseTransport):
    def __init__(self) -> None:
        self.requests: list[httpx.Request] = []

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        if request.method == "POST" and request.url.path == "/pty":
            assert request.headers["x-route"] == "r"
            assert json.loads(request.content) == {
                "cwd": "/workspace",
                "command": "bash",
            }
            return httpx.Response(201, json={"session_id": "pty-1"}, request=request)

        if request.method == "GET" and request.url.path == "/pty/pty-1":
            return httpx.Response(
                200,
                json={
                    "session_id": "pty-1",
                    "running": True,
                    "output_offset": 7,
                },
                request=request,
            )

        if request.method == "DELETE" and request.url.path == "/pty/pty-1":
            return httpx.Response(204, request=request)

        return httpx.Response(404, request=request)


class _SyncPTYTransport(httpx.BaseTransport):
    def __init__(self) -> None:
        self.requests: list[httpx.Request] = []

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        if request.method == "POST" and request.url.path == "/pty":
            assert request.headers["x-route"] == "r"
            assert json.loads(request.content) == {"command": "bash"}
            return httpx.Response(201, json={"session_id": "pty-1"}, request=request)

        if request.method == "GET" and request.url.path == "/pty/pty-1":
            return httpx.Response(
                200,
                json={
                    "session_id": "pty-1",
                    "running": False,
                    "output_offset": 3,
                },
                request=request,
            )

        if request.method == "DELETE" and request.url.path == "/pty/pty-1":
            return httpx.Response(204, request=request)

        return httpx.Response(404, request=request)


@pytest.mark.asyncio
async def test_async_pty_adapter_rest_and_websocket_url() -> None:
    transport = _AsyncPTYTransport()
    config = ConnectionConfig(protocol="http", transport=transport)
    endpoint = SandboxEndpoint(endpoint="sandbox.local:44772", headers={"X-Route": "r"})
    adapter = PTYAdapter(config, endpoint)

    session_id = await adapter.create_session(cwd="/workspace", command="bash")
    status = await adapter.get_session_status(session_id)
    await adapter.delete_session(session_id)
    await adapter._httpx_client.aclose()

    assert session_id == "pty-1"
    assert status.session_id == "pty-1"
    assert status.running is True
    assert status.output_offset == 7
    assert [request.method for request in transport.requests] == [
        "POST",
        "GET",
        "DELETE",
    ]
    assert adapter._build_ws_url(
        "pty-1",
        pty=False,
        since=7,
        takeover=True,
    ) == "ws://sandbox.local:44772/pty/pty-1/ws?pty=0&since=7&takeover=1"


@pytest.mark.asyncio
async def test_async_pty_connect_websocket_uses_built_url(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    config = ConnectionConfig(protocol="https")
    endpoint = SandboxEndpoint(endpoint="sandbox.local:44772")
    adapter = PTYAdapter(config, endpoint)
    called: dict[str, str] = {}

    async def fake_connect(url: str) -> str:
        called["url"] = url
        return "connection"

    monkeypatch.setattr(adapter, "_connect_websocket", fake_connect)

    connection = await adapter.connect_websocket("pty-1", since=2, takeover=True)
    await adapter._httpx_client.aclose()

    assert connection == "connection"
    assert called["url"] == "wss://sandbox.local:44772/pty/pty-1/ws?since=2&takeover=1"


def test_sync_pty_adapter_rest_and_websocket_url() -> None:
    transport = _SyncPTYTransport()
    config = ConnectionConfigSync(protocol="http", transport=transport)
    endpoint = SandboxEndpoint(endpoint="sandbox.local:44772", headers={"X-Route": "r"})
    adapter = PTYAdapterSync(config, endpoint)

    session_id = adapter.create_session(command="bash")
    status = adapter.get_session_status(session_id)
    adapter.delete_session(session_id)
    adapter._httpx_client.close()

    assert session_id == "pty-1"
    assert status.session_id == "pty-1"
    assert status.running is False
    assert status.output_offset == 3
    assert [request.method for request in transport.requests] == [
        "POST",
        "GET",
        "DELETE",
    ]
    assert adapter._build_ws_url(
        "pty-1",
        pty=False,
        since=7,
        takeover=True,
    ) == "ws://sandbox.local:44772/pty/pty-1/ws?pty=0&since=7&takeover=1"


def test_pty_binary_stdin_encoding() -> None:
    assert encode_pty_stdin("ls\n") == b"\x00ls\n"
    assert encode_pty_stdin(b"pwd\n") == b"\x00pwd\n"


def test_sandbox_exposes_injected_pty_services() -> None:
    pty_service = object()
    sandbox = Sandbox(
        sandbox_id="sandbox-1",
        sandbox_service=object(),
        filesystem_service=object(),
        command_service=object(),
        health_service=object(),
        metrics_service=object(),
        egress_service=object(),
        connection_config=ConnectionConfig(),
        pty_service=pty_service,
    )
    sync_sandbox = SandboxSync(
        sandbox_id="sandbox-1",
        sandbox_service=object(),
        filesystem_service=object(),
        command_service=object(),
        health_service=object(),
        metrics_service=object(),
        egress_service=object(),
        connection_config=ConnectionConfigSync(),
        pty_service=pty_service,
    )

    assert sandbox.pty is pty_service
    assert sync_sandbox.pty is pty_service
