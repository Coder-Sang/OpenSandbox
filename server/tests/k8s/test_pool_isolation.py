# Copyright 2026 Alibaba Group Holding Ltd.
# Licensed under the Apache License, Version 2.0

import json

import pytest

from pydantic import ValidationError

from opensandbox_server.api.schema import CreateSandboxRequest, SandboxIsolation
from opensandbox_server.services.k8s.pool_isolation import (
    BWRAP_MOUNT_POLICY_ANNOTATION,
    EXECUTION_ISOLATION_ANNOTATION,
    PoolIsolationError,
    validate_pool_isolation,
)


def _pool(*, recycle: str = "Delete") -> dict:
    policy = {
        "version": 1,
        "roots": {
            "projects": {
                "mountRoot": "/storage",
                "source": "/storage/projects",
                "targetPrefixes": ["/workspace"],
                "maxMode": "rw",
            },
            "shared": {
                "mountRoot": "/shared-storage",
                "source": "/shared-storage/sdk",
                "targetPrefixes": ["/workspace"],
                "maxMode": "ro",
            },
        },
    }
    return {
        "spec": {
            "recycleStrategy": {"type": recycle},
            "template": {
                "metadata": {
                    "annotations": {
                        EXECUTION_ISOLATION_ANNOTATION: "bwrap-v1",
                        BWRAP_MOUNT_POLICY_ANNOTATION: json.dumps(policy),
                    }
                },
                "spec": {
                    "automountServiceAccountToken": False,
                    "volumes": [
                        {"name": "projects", "persistentVolumeClaim": {"claimName": "data"}},
                        {"name": "shared", "persistentVolumeClaim": {"claimName": "shared"}},
                    ],
                    "containers": [
                        {
                            "name": "task-executor",
                            "env": [
                                {
                                    "name": "EXECD_ACCESS_TOKEN",
                                    "valueFrom": {"secretKeyRef": {"name": "auth", "key": "execd"}},
                                },
                                {
                                    "name": "TASK_EXECUTOR_AUTH_TOKEN",
                                    "valueFrom": {"secretKeyRef": {"name": "auth", "key": "tasks"}},
                                },
                            ],
                            "volumeMounts": [
                                {"name": "projects", "mountPath": "/storage"},
                                {"name": "shared", "mountPath": "/shared-storage"},
                            ],
                        }
                    ],
                },
            },
        }
    }


def _isolation(**overrides) -> SandboxIsolation:
    mount = {
        "root": "projects",
        "subPath": "project-A",
        "target": "/workspace/a",
        "mode": "rw",
        **overrides,
    }
    return SandboxIsolation.model_validate({"type": "bwrap", "mounts": [mount]})


def test_valid_policy_returns_internal_runtime_binding() -> None:
    result = validate_pool_isolation(_pool(), _isolation())
    assert result == {
        "type": "bwrap",
        "roots": json.loads(
            _pool()["spec"]["template"]["metadata"]["annotations"][BWRAP_MOUNT_POLICY_ANNOTATION]
        )["roots"],
        "mounts": [
            {"root": "projects", "subPath": "project-A", "target": "/workspace/a", "mode": "rw"}
        ],
    }


def test_policy_allows_nested_targets_and_sorts_parent_first() -> None:
    isolation = SandboxIsolation.model_validate(
        {
            "type": "bwrap",
            "mounts": [
                {
                    "root": "shared",
                    "subPath": "sdk",
                    "target": "/workspace/cache/sdk",
                    "mode": "ro",
                },
                {
                    "root": "projects",
                    "subPath": "project-A-cache",
                    "target": "/workspace/cache",
                    "mode": "rw",
                },
                {
                    "root": "projects",
                    "subPath": "project-Z",
                    "target": "/workspace/z",
                    "mode": "rw",
                },
                {
                    "root": "projects",
                    "subPath": "project-A",
                    "target": "/workspace",
                    "mode": "ro",
                },
            ],
        }
    )

    result = validate_pool_isolation(_pool(), isolation)

    assert result is not None
    assert [mount["target"] for mount in result["mounts"]] == [
        "/workspace",
        "/workspace/cache",
        "/workspace/z",
        "/workspace/cache/sdk",
    ]
    assert result["mounts"][1]["mode"] == "rw"


def test_policy_rejects_duplicate_target() -> None:
    isolation = SandboxIsolation.model_validate(
        {
            "type": "bwrap",
            "mounts": [
                {
                    "root": "projects",
                    "subPath": "project-A",
                    "target": "/workspace",
                    "mode": "rw",
                },
                {
                    "root": "shared",
                    "subPath": "sdk",
                    "target": "/workspace",
                    "mode": "ro",
                },
            ],
        }
    )

    with pytest.raises(PoolIsolationError, match="duplicate mount target"):
        validate_pool_isolation(_pool(), isolation)


@pytest.mark.parametrize(
    ("isolation", "message"),
    [
        (None, "requires isolation"),
        (_isolation(root="shared", mode="rw"), "does not allow rw"),
        (_isolation(target="/opt/opensandbox/pwn"), "outside root"),
    ],
)
def test_policy_rejects_unsafe_requests(isolation, message: str) -> None:
    with pytest.raises(PoolIsolationError, match=message):
        validate_pool_isolation(_pool(), isolation)


def test_policy_requires_delete_recycling() -> None:
    with pytest.raises(PoolIsolationError, match="recycleStrategy"):
        validate_pool_isolation(_pool(recycle="Noop"), _isolation())


def test_ordinary_pool_rejects_isolation() -> None:
    pool = _pool()
    pool["spec"]["template"]["metadata"]["annotations"].pop(EXECUTION_ISOLATION_ANNOTATION)
    with pytest.raises(PoolIsolationError, match="accepted only"):
        validate_pool_isolation(pool, _isolation())


def test_policy_rejects_same_pvc_at_second_path() -> None:
    pool = _pool()
    pool["spec"]["template"]["spec"]["volumes"].append(
        {"name": "projects-alias", "persistentVolumeClaim": {"claimName": "data"}}
    )
    pool["spec"]["template"]["spec"]["containers"][0]["volumeMounts"].append(
        {"name": "projects-alias", "mountPath": "/leaked-storage"}
    )
    with pytest.raises(PoolIsolationError, match="undeclared path"):
        validate_pool_isolation(pool, _isolation())


def test_policy_rejects_projected_service_account_token() -> None:
    pool = _pool()
    pool["spec"]["template"]["spec"]["volumes"].append(
        {
            "name": "token",
            "projected": {"sources": [{"serviceAccountToken": {"path": "token"}}]},
        }
    )
    with pytest.raises(PoolIsolationError, match="ServiceAccount"):
        validate_pool_isolation(pool, _isolation())


def test_policy_rejects_control_directory_as_mount_root() -> None:
    pool = _pool()
    policy = json.loads(
        pool["spec"]["template"]["metadata"]["annotations"][BWRAP_MOUNT_POLICY_ANNOTATION]
    )
    policy["roots"]["projects"]["mountRoot"] = "/opt"
    policy["roots"]["projects"]["source"] = "/opt/projects"
    pool["spec"]["template"]["metadata"]["annotations"][BWRAP_MOUNT_POLICY_ANNOTATION] = json.dumps(
        policy
    )
    pool["spec"]["template"]["spec"]["containers"][0]["volumeMounts"][0]["mountPath"] = "/opt"
    with pytest.raises(PoolIsolationError, match="control directory"):
        validate_pool_isolation(pool, _isolation())


def test_request_rejects_isolation_without_pool_ref() -> None:
    with pytest.raises(ValidationError, match="only together with extensions.poolRef"):
        CreateSandboxRequest.model_validate(
            {
                "image": {"uri": "python:3.13"},
                "entrypoint": ["python"],
                "resourceLimits": {"cpu": "1", "memory": "1Gi"},
                "isolation": {
                    "type": "bwrap",
                    "mounts": [
                        {
                            "root": "projects",
                            "subPath": "project-A",
                            "target": "/workspace/a",
                            "mode": "rw",
                        }
                    ],
                },
            }
        )


def test_request_allows_lifecycle_for_isolated_pool() -> None:
    request = CreateSandboxRequest.model_validate(
        {
            "extensions": {"poolRef": "secure"},
            "lifecycle": {"preStart": {"command": ["prepare"]}},
            "isolation": {
                "type": "bwrap",
                "mounts": [
                    {
                        "root": "projects",
                        "subPath": "project-A",
                        "target": "/workspace/a",
                        "mode": "rw",
                    }
                ],
            },
        }
    )
    assert request.lifecycle is not None


def test_request_rejects_isolation_in_template_mode() -> None:
    with pytest.raises(ValidationError, match="templateId cannot be combined with: isolation"):
        CreateSandboxRequest.model_validate(
            {
                "templateId": "golden-template",
                "timeout": 300,
                "isolation": {"type": "bwrap", "mounts": []},
            }
        )
