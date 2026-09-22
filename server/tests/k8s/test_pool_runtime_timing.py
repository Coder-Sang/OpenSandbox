"""Pool runtime timing and one-shot initialization regressions."""

from types import SimpleNamespace
from unittest.mock import AsyncMock, MagicMock, patch

import httpx
import pytest

from opensandbox_server.api.schema import CreateSandboxRequest, Endpoint
from opensandbox_server.services.k8s.kubernetes_service import PoolIsolationError


@pytest.mark.asyncio
async def test_pool_init_logs_phases_and_reconciles_lost_post_response(k8s_service):
    """A timed-out init POST still succeeds when the matching ready identity appears."""
    sandbox_id = "sandbox-timing"
    k8s_service.workload_provider.get_internal_endpoint.return_value = Endpoint(
        endpoint="10.0.0.9:44772"
    )
    client = MagicMock()
    client.__aenter__ = AsyncMock(return_value=client)
    client.__aexit__ = AsyncMock(return_value=None)
    client.get = AsyncMock(
        side_effect=[
            SimpleNamespace(is_success=True),
            SimpleNamespace(
                status_code=200,
                json=lambda: {
                    "initialized": True,
                    "sandboxId": sandbox_id,
                    "generation": 1,
                },
            ),
        ]
    )
    client.post = AsyncMock(side_effect=httpx.ReadTimeout("response lost"))

    with (
        patch(
            "opensandbox_server.services.k8s.kubernetes_service.httpx.AsyncClient",
            return_value=client,
        ),
        patch("opensandbox_server.services.k8s.kubernetes_service.logger.info") as log_info,
    ):
        await k8s_service._initialize_pool_bwrap_runtime(
            {"metadata": {"generation": 1}},
            CreateSandboxRequest.model_validate({"extensions": {"poolRef": "secure"}}),
            sandbox_id,
            {"mounts": [{"target": "/workspace"}]},
            "secret-control-token",
            "secret-business-token",
        )

    call = next(call for call in log_info.call_args_list if "Pool runtime init" in call.args[0])
    message = call.args[0] % call.args[1:]
    assert "mount_count=1 outcome=success" in message
    assert "post_error_type=ReadTimeout" in message
    assert "secret-control-token" not in message
    assert "secret-business-token" not in message
    assert "/workspace" not in message
    assert client.post.await_count == 1


@pytest.mark.asyncio
async def test_pool_init_failure_logs_phase_without_payload(k8s_service):
    """Identity mismatch remains fatal and emits bounded diagnostics."""
    k8s_service.workload_provider.get_internal_endpoint.return_value = Endpoint(
        endpoint="10.0.0.9:44772"
    )
    client = MagicMock()
    client.__aenter__ = AsyncMock(return_value=client)
    client.__aexit__ = AsyncMock(return_value=None)
    client.get = AsyncMock(
        side_effect=[
            SimpleNamespace(is_success=True),
            SimpleNamespace(
                status_code=200,
                json=lambda: {
                    "initialized": True,
                    "sandboxId": "another-sandbox",
                    "generation": 1,
                },
            ),
        ]
    )
    client.post = AsyncMock(return_value=SimpleNamespace(status_code=200))

    with (
        patch(
            "opensandbox_server.services.k8s.kubernetes_service.httpx.AsyncClient",
            return_value=client,
        ),
        patch("opensandbox_server.services.k8s.kubernetes_service.logger.info") as log_info,
        pytest.raises(PoolIsolationError, match="identity does not match"),
    ):
        await k8s_service._initialize_pool_bwrap_runtime(
            {"metadata": {"generation": 1}},
            CreateSandboxRequest.model_validate({"extensions": {"poolRef": "secure"}}),
            "sandbox-timing",
            {"mounts": [{"target": "/workspace"}]},
            "secret-control-token",
            "secret-business-token",
        )

    call = next(call for call in log_info.call_args_list if "Pool runtime init" in call.args[0])
    message = call.args[0] % call.args[1:]
    assert "outcome=failure error_type=PoolIsolationError" in message
    assert "ready_ms=" in message
    assert "secret-control-token" not in message
    assert "/workspace" not in message
