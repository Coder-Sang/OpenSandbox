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
import httpx
import pytest

from opensandbox.adapters.command_adapter import CommandsAdapter
from opensandbox.adapters.health_adapter import HealthAdapter
from opensandbox.config import ConnectionConfig, ConnectionConfigSync
from opensandbox.models.sandboxes import SandboxEndpoint
from opensandbox.sync.adapters.command_adapter import CommandsAdapterSync
from opensandbox.sync.adapters.health_adapter import HealthAdapterSync


class _RedirectTransport(httpx.BaseTransport):
    def __init__(self) -> None:
        self.paths: list[str] = []

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        self.paths.append(request.url.path)
        if request.url.path == "/start":
            return httpx.Response(
                307,
                headers={"Location": "/final"},
                request=request,
            )
        return httpx.Response(204, request=request)


class _AsyncRedirectTransport(httpx.AsyncBaseTransport):
    def __init__(self) -> None:
        self.paths: list[str] = []

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        self.paths.append(request.url.path)
        if request.url.path == "/start":
            return httpx.Response(
                307,
                headers={"Location": "/final"},
                request=request,
            )
        return httpx.Response(204, request=request)


def test_follow_redirects_defaults_to_false() -> None:
    assert ConnectionConfig().follow_redirects is False
    assert ConnectionConfigSync().follow_redirects is False


@pytest.mark.asyncio
async def test_async_adapter_http_client_follows_redirects_from_config() -> None:
    transport = _AsyncRedirectTransport()
    cfg = ConnectionConfig(protocol="http", transport=transport, follow_redirects=True)
    adapter = HealthAdapter(cfg, SandboxEndpoint(endpoint="sandbox.local:8080", port=8080))

    response = await adapter._httpx_client.get("/start")
    await adapter._httpx_client.aclose()

    assert response.status_code == 204
    assert [response.status_code for response in response.history] == [307]
    assert transport.paths == ["/start", "/final"]


@pytest.mark.asyncio
async def test_async_sse_client_follows_redirects_from_config() -> None:
    transport = _AsyncRedirectTransport()
    cfg = ConnectionConfig(protocol="http", transport=transport, follow_redirects=True)
    adapter = CommandsAdapter(cfg, SandboxEndpoint(endpoint="sandbox.local:8080", port=8080))

    response = await adapter._sse_client.get("http://sandbox.local:8080/start")
    await adapter._httpx_client.aclose()
    await adapter._sse_client.aclose()

    assert response.status_code == 204
    assert [response.status_code for response in response.history] == [307]
    assert transport.paths == ["/start", "/final"]


def test_sync_adapter_http_client_follows_redirects_from_config() -> None:
    transport = _RedirectTransport()
    cfg = ConnectionConfigSync(protocol="http", transport=transport, follow_redirects=True)
    adapter = HealthAdapterSync(
        cfg,
        SandboxEndpoint(endpoint="sandbox.local:8080", port=8080),
    )

    response = adapter._httpx_client.get("/start")
    adapter._httpx_client.close()

    assert response.status_code == 204
    assert [response.status_code for response in response.history] == [307]
    assert transport.paths == ["/start", "/final"]


def test_sync_sse_client_follows_redirects_from_config() -> None:
    transport = _RedirectTransport()
    cfg = ConnectionConfigSync(protocol="http", transport=transport, follow_redirects=True)
    adapter = CommandsAdapterSync(
        cfg,
        SandboxEndpoint(endpoint="sandbox.local:8080", port=8080),
    )

    response = adapter._sse_client.get("http://sandbox.local:8080/start")
    adapter._httpx_client.close()
    adapter._sse_client.close()

    assert response.status_code == 204
    assert [response.status_code for response in response.history] == [307]
    assert transport.paths == ["/start", "/final"]
