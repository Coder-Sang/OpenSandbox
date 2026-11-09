# Copyright 2026 The OpenSandbox Authors
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

"""Create-time backend routing for the composite sandbox service."""

from __future__ import annotations

import asyncio

from opensandbox_server.api.schema import CreateSandboxRequest, ImageSpec, ResourceLimits
from opensandbox_server.services.composite_service import CompositeSandboxService


class _RecordingBackend:
    def __init__(self, name: str) -> None:
        self.name = name
        self.requests: list[CreateSandboxRequest] = []

    async def create_sandbox(self, request: CreateSandboxRequest) -> object:
        self.requests.append(request)
        return f"created-by-{self.name}"


def _create(*, snapshot_id: str | None = None, template_id: str | None = None) -> CreateSandboxRequest:
    if template_id is not None:
        # Template mode rejects every workload-shaping field by schema.
        return CreateSandboxRequest.model_construct(
            template_id=template_id,
            timeout=3600,
        )
    return CreateSandboxRequest.model_construct(
        image=None if snapshot_id else ImageSpec(uri="registry.example.com/app:1"),
        snapshot_id=snapshot_id,
        timeout=3600,
        entrypoint=None if snapshot_id else ["tail", "-f", "/dev/null"],
        resource_limits=ResourceLimits(root={"cpu": "500m", "memory": "512Mi"}),
    )


def _run(composite: CompositeSandboxService, request: CreateSandboxRequest) -> object:
    return asyncio.run(composite.create_sandbox(request))


def test_snapshot_restore_from_fsb_backend_routes_to_fsb() -> None:
    kubernetes = _RecordingBackend("kubernetes")
    fsb = _RecordingBackend("fsb")
    composite = CompositeSandboxService(kubernetes=kubernetes, fsb=fsb)  # type: ignore[arg-type]
    request = _create(snapshot_id="snap-1")
    request._resolved_snapshot_backend = "fsb"

    result = _run(composite, request)

    assert result == "created-by-fsb"
    assert fsb.requests and kubernetes.requests == []


def test_snapshot_restore_from_default_backend_stays_with_pod_backend() -> None:
    kubernetes = _RecordingBackend("kubernetes")
    fsb = _RecordingBackend("fsb")
    composite = CompositeSandboxService(kubernetes=kubernetes, fsb=fsb)  # type: ignore[arg-type]
    request = _create(snapshot_id="snap-2")
    request._resolved_snapshot_backend = None

    result = _run(composite, request)

    assert result == "created-by-kubernetes"
    assert kubernetes.requests and fsb.requests == []


def test_unresolved_snapshot_id_stays_with_pod_backend() -> None:
    kubernetes = _RecordingBackend("kubernetes")
    fsb = _RecordingBackend("fsb")
    composite = CompositeSandboxService(kubernetes=kubernetes, fsb=fsb)  # type: ignore[arg-type]
    request = _create(snapshot_id="snap-3")

    result = _run(composite, request)

    assert result == "created-by-kubernetes"
    assert kubernetes.requests and fsb.requests == []


def test_template_id_still_routes_to_fsb() -> None:
    kubernetes = _RecordingBackend("kubernetes")
    fsb = _RecordingBackend("fsb")
    composite = CompositeSandboxService(kubernetes=kubernetes, fsb=fsb)  # type: ignore[arg-type]

    result = _run(composite, _create(template_id="tpl-1"))

    assert result == "created-by-fsb"
    assert fsb.requests and kubernetes.requests == []


def test_context_routes_by_sandbox_backend():
    from unittest.mock import MagicMock
    kubernetes, fsb = MagicMock(), MagicMock()
    composite = CompositeSandboxService(kubernetes=kubernetes, fsb=fsb)
    composite.get_sandbox_diagnostic_context("sbx-1")
    composite.get_sandbox_diagnostic_context("fsb-1")
    kubernetes.get_sandbox_diagnostic_context.assert_called_once_with("sbx-1")
    fsb.get_sandbox_diagnostic_context.assert_called_once_with("fsb-1")


def test_unsupported_context_checks_lookup_before_501():
    from types import SimpleNamespace
    from unittest.mock import MagicMock
    import pytest
    from fastapi import HTTPException
    from opensandbox_server.services.sandbox_service import SandboxService
    backend = SimpleNamespace(get_sandbox=MagicMock())
    with pytest.raises(HTTPException) as exc:
        SandboxService.get_sandbox_diagnostic_context(backend, "sbx-1")
    assert exc.value.status_code == 501
    backend.get_sandbox.assert_called_once_with("sbx-1")
    backend.get_sandbox.side_effect = HTTPException(404, "not found")
    with pytest.raises(HTTPException) as exc:
        SandboxService.get_sandbox_diagnostic_context(backend, "missing")
    assert exc.value.status_code == 404
