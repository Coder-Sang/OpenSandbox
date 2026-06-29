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
"""httpx helpers shared by handwritten SDK adapters.

These helpers preserve user-provided event hooks while appending the SDK's
request hook that strips `OPEN-SANDBOX-API-KEY` from cross-origin redirects.
The SDK hook must run last so user hooks cannot accidentally re-add the custom
API key before httpx sends the redirected request.
"""

from collections.abc import Callable, Mapping, Sequence
from typing import Any

import httpx

OPEN_SANDBOX_API_KEY_HEADER = "OPEN-SANDBOX-API-KEY"
EventHook = Callable[..., Any]
EventHooks = Mapping[str, Sequence[EventHook]]

_DEFAULT_PORTS = {
    "http": 80,
    "https": 443,
}


def _origin(url: httpx.URL) -> tuple[str, str | None, int | None]:
    return (url.scheme, url.host, url.port or _DEFAULT_PORTS.get(url.scheme))


def _strip_api_key_for_cross_origin_request(
    request: httpx.Request,
    *,
    base_origin: tuple[str, str | None, int | None],
) -> None:
    if _origin(request.url) == base_origin:
        return
    if OPEN_SANDBOX_API_KEY_HEADER in request.headers:
        del request.headers[OPEN_SANDBOX_API_KEY_HEADER]


def _copy_event_hooks(event_hooks: EventHooks | None) -> dict[str, list[EventHook]]:
    if event_hooks is None:
        return {}
    return {event: list(hooks) for event, hooks in event_hooks.items()}


def build_api_key_redirect_event_hooks(
    base_url: str,
    event_hooks: EventHooks | None = None,
) -> dict[str, list[EventHook]]:
    """Build sync hooks for SDK clients.

    Existing user hooks are copied and preserved. The SDK request hook is
    appended after user request hooks so `OPEN-SANDBOX-API-KEY` is removed from
    any request whose origin differs from `base_url`.
    """
    base_origin = _origin(httpx.URL(base_url))

    def strip_api_key(request: httpx.Request) -> None:
        _strip_api_key_for_cross_origin_request(request, base_origin=base_origin)

    hooks = _copy_event_hooks(event_hooks)
    hooks.setdefault("request", []).append(strip_api_key)
    return hooks


def build_async_api_key_redirect_event_hooks(
    base_url: str,
    event_hooks: EventHooks | None = None,
) -> dict[str, list[EventHook]]:
    """Build async hooks for SDK clients.

    Existing user hooks are copied and preserved. The SDK request hook is
    appended after user request hooks so `OPEN-SANDBOX-API-KEY` is removed from
    any request whose origin differs from `base_url`.
    """
    base_origin = _origin(httpx.URL(base_url))

    async def strip_api_key(request: httpx.Request) -> None:
        _strip_api_key_for_cross_origin_request(request, base_origin=base_origin)

    hooks = _copy_event_hooks(event_hooks)
    hooks.setdefault("request", []).append(strip_api_key)
    return hooks
