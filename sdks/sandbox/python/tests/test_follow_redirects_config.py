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
from opensandbox.adapters.sandboxes_adapter import SandboxesAdapter
from opensandbox.config import ConnectionConfig, ConnectionConfigSync
from opensandbox.models.sandboxes import SandboxEndpoint
from opensandbox.sync.adapters.command_adapter import CommandsAdapterSync
from opensandbox.sync.adapters.health_adapter import HealthAdapterSync
from opensandbox.sync.adapters.sandboxes_adapter import SandboxesAdapterSync


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


class _CrossOriginRedirectTransport(httpx.BaseTransport):
    def __init__(self) -> None:
        self.requests: list[tuple[str | None, str, str | None]] = []

    def handle_request(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(
            (
                request.url.host,
                request.url.path,
                request.headers.get("OPEN-SANDBOX-API-KEY"),
            )
        )
        if request.url.path.endswith("/start"):
            return httpx.Response(
                307,
                headers={"Location": "http://redirect.local/final"},
                request=request,
            )
        return httpx.Response(204, request=request)


class _AsyncCrossOriginRedirectTransport(httpx.AsyncBaseTransport):
    def __init__(self) -> None:
        self.requests: list[tuple[str | None, str, str | None]] = []

    async def handle_async_request(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(
            (
                request.url.host,
                request.url.path,
                request.headers.get("OPEN-SANDBOX-API-KEY"),
            )
        )
        if request.url.path.endswith("/start"):
            return httpx.Response(
                307,
                headers={"Location": "http://redirect.local/final"},
                request=request,
            )
        return httpx.Response(204, request=request)


def test_follow_redirects_defaults_to_false() -> None:
    assert ConnectionConfig().follow_redirects is False
    assert ConnectionConfigSync().follow_redirects is False
    assert ConnectionConfig().event_hooks == {}
    assert ConnectionConfigSync().event_hooks == {}


@pytest.mark.asyncio
async def test_async_adapter_http_client_follows_redirects_from_config() -> None:
    transport = _AsyncRedirectTransport()
    cfg = ConnectionConfig(protocol="http", transport=transport, follow_redirects=True)
    adapter = HealthAdapter(cfg, SandboxEndpoint(endpoint="sandbox.local:8080"))

    response = await adapter._httpx_client.get("/start")
    await adapter._httpx_client.aclose()

    assert response.status_code == 204
    assert [response.status_code for response in response.history] == [307]
    assert transport.paths == ["/start", "/final"]


@pytest.mark.asyncio
async def test_async_sse_client_follows_redirects_from_config() -> None:
    transport = _AsyncRedirectTransport()
    cfg = ConnectionConfig(protocol="http", transport=transport, follow_redirects=True)
    adapter = CommandsAdapter(cfg, SandboxEndpoint(endpoint="sandbox.local:8080"))

    response = await adapter._sse_client.get("http://sandbox.local:8080/start")
    await adapter._httpx_client.aclose()
    await adapter._sse_client.aclose()

    assert response.status_code == 204
    assert [response.status_code for response in response.history] == [307]
    assert transport.paths == ["/start", "/final"]


@pytest.mark.asyncio
async def test_async_adapter_strips_api_key_on_cross_origin_redirect() -> None:
    transport = _AsyncCrossOriginRedirectTransport()
    cfg = ConnectionConfig(
        api_key="secret",
        domain="sandbox.local:8080",
        protocol="http",
        transport=transport,
        follow_redirects=True,
    )
    adapter = SandboxesAdapter(cfg)

    response = await adapter._httpx_client.get("/start")
    await adapter._httpx_client.aclose()

    assert response.status_code == 204
    assert transport.requests == [
        ("sandbox.local", "/v1/start", "secret"),
        ("redirect.local", "/final", None),
    ]


@pytest.mark.asyncio
async def test_async_sse_client_strips_api_key_on_cross_origin_redirect() -> None:
    transport = _AsyncCrossOriginRedirectTransport()
    cfg = ConnectionConfig(
        headers={"OPEN-SANDBOX-API-KEY": "secret"},
        protocol="http",
        transport=transport,
        follow_redirects=True,
    )
    adapter = CommandsAdapter(cfg, SandboxEndpoint(endpoint="sandbox.local:8080"))

    response = await adapter._sse_client.get("http://sandbox.local:8080/start")
    await adapter._httpx_client.aclose()
    await adapter._sse_client.aclose()

    assert response.status_code == 204
    assert transport.requests == [
        ("sandbox.local", "/start", "secret"),
        ("redirect.local", "/final", None),
    ]


@pytest.mark.asyncio
async def test_async_event_hooks_are_preserved_and_strip_hook_runs_last() -> None:
    transport = _AsyncCrossOriginRedirectTransport()
    request_hosts: list[str | None] = []
    response_statuses: list[int] = []

    async def request_hook(request: httpx.Request) -> None:
        request_hosts.append(request.url.host)
        request.headers["OPEN-SANDBOX-API-KEY"] = "hook-secret"

    async def response_hook(response: httpx.Response) -> None:
        response_statuses.append(response.status_code)

    cfg = ConnectionConfig(
        api_key="secret",
        domain="sandbox.local:8080",
        protocol="http",
        transport=transport,
        follow_redirects=True,
        event_hooks={"request": [request_hook], "response": [response_hook]},
    )
    adapter = SandboxesAdapter(cfg)

    response = await adapter._httpx_client.get("/start")
    await adapter._httpx_client.aclose()

    assert response.status_code == 204
    assert request_hosts == ["sandbox.local", "redirect.local"]
    assert response_statuses == [307, 204]
    assert transport.requests == [
        ("sandbox.local", "/v1/start", "hook-secret"),
        ("redirect.local", "/final", None),
    ]


def test_sync_adapter_http_client_follows_redirects_from_config() -> None:
    transport = _RedirectTransport()
    cfg = ConnectionConfigSync(protocol="http", transport=transport, follow_redirects=True)
    adapter = HealthAdapterSync(
        cfg,
        SandboxEndpoint(endpoint="sandbox.local:8080"),
    )

    response = adapter._httpx_client.get("/start")
    adapter._httpx_client.close()

    assert response.status_code == 204
    assert [response.status_code for response in response.history] == [307]
    assert transport.paths == ["/start", "/final"]


def test_sync_adapter_strips_api_key_on_cross_origin_redirect() -> None:
    transport = _CrossOriginRedirectTransport()
    cfg = ConnectionConfigSync(
        api_key="secret",
        domain="sandbox.local:8080",
        protocol="http",
        transport=transport,
        follow_redirects=True,
    )
    adapter = SandboxesAdapterSync(cfg)

    response = adapter._httpx_client.get("/start")
    adapter._httpx_client.close()

    assert response.status_code == 204
    assert transport.requests == [
        ("sandbox.local", "/v1/start", "secret"),
        ("redirect.local", "/final", None),
    ]


def test_sync_sse_client_strips_api_key_on_cross_origin_redirect() -> None:
    transport = _CrossOriginRedirectTransport()
    cfg = ConnectionConfigSync(
        headers={"OPEN-SANDBOX-API-KEY": "secret"},
        protocol="http",
        transport=transport,
        follow_redirects=True,
    )
    adapter = CommandsAdapterSync(
        cfg,
        SandboxEndpoint(endpoint="sandbox.local:8080"),
    )

    response = adapter._sse_client.get("http://sandbox.local:8080/start")
    adapter._httpx_client.close()
    adapter._sse_client.close()

    assert response.status_code == 204
    assert transport.requests == [
        ("sandbox.local", "/start", "secret"),
        ("redirect.local", "/final", None),
    ]


def test_sync_event_hooks_are_preserved_and_strip_hook_runs_last() -> None:
    transport = _CrossOriginRedirectTransport()
    request_hosts: list[str | None] = []
    response_statuses: list[int] = []

    def request_hook(request: httpx.Request) -> None:
        request_hosts.append(request.url.host)
        request.headers["OPEN-SANDBOX-API-KEY"] = "hook-secret"

    def response_hook(response: httpx.Response) -> None:
        response_statuses.append(response.status_code)

    cfg = ConnectionConfigSync(
        api_key="secret",
        domain="sandbox.local:8080",
        protocol="http",
        transport=transport,
        follow_redirects=True,
        event_hooks={"request": [request_hook], "response": [response_hook]},
    )
    adapter = SandboxesAdapterSync(cfg)

    response = adapter._httpx_client.get("/start")
    adapter._httpx_client.close()

    assert response.status_code == 204
    assert request_hosts == ["sandbox.local", "redirect.local"]
    assert response_statuses == [307, 204]
    assert transport.requests == [
        ("sandbox.local", "/v1/start", "hook-secret"),
        ("redirect.local", "/final", None),
    ]


def test_sync_sse_client_follows_redirects_from_config() -> None:
    transport = _RedirectTransport()
    cfg = ConnectionConfigSync(protocol="http", transport=transport, follow_redirects=True)
    adapter = CommandsAdapterSync(
        cfg,
        SandboxEndpoint(endpoint="sandbox.local:8080"),
    )

    response = adapter._sse_client.get("http://sandbox.local:8080/start")
    adapter._httpx_client.close()
    adapter._sse_client.close()

    assert response.status_code == 204
    assert [response.status_code for response in response.history] == [307]
    assert transport.paths == ["/start", "/final"]
