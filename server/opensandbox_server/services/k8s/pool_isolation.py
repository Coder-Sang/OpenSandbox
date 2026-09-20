# Copyright 2026 Alibaba Group Holding Ltd.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.

"""Validation for the trusted bwrap-v1 Kubernetes Pool contract."""

from __future__ import annotations

import json
import posixpath
from typing import Any, Dict

from opensandbox_server.api.schema import SandboxIsolation

EXECUTION_ISOLATION_ANNOTATION = "opensandbox.io/execution-isolation"
BWRAP_MOUNT_POLICY_ANNOTATION = "opensandbox.io/bwrap-mount-policy"
BWRAP_V1 = "bwrap-v1"
INTERNAL_BWRAP_EXTENSION = "opensandbox.io/internal-bwrap-v1"

_CONTROL_TARGETS = (
    "/opt/opensandbox",
    "/run/execd",
    "/proc",
    "/sys",
    "/dev",
    "/var/run/secrets",
)


class PoolIsolationError(ValueError):
    """The Pool or request violates the bwrap-v1 trust contract."""


def _within(parent: str, child: str) -> bool:
    parent = parent.rstrip("/") or "/"
    return child == parent or child.startswith(parent + "/")


def _absolute_clean(path: object) -> bool:
    return isinstance(path, str) and path.startswith("/") and posixpath.normpath(path) == path


def _sandbox_container(template_spec: Dict[str, Any]) -> Dict[str, Any]:
    containers = template_spec.get("containers") or []
    if not isinstance(containers, list) or not containers:
        raise PoolIsolationError("bwrap-v1 Pool template must contain a sandbox container")
    for container in containers:
        if isinstance(container, dict) and container.get("name") in {
            "sandbox",
            "sandbox-container",
        }:
            return container
    if not isinstance(containers[0], dict):
        raise PoolIsolationError("bwrap-v1 Pool sandbox container is invalid")
    return containers[0]


def validate_pool_isolation(
    pool: Dict[str, Any], isolation: SandboxIsolation | None
) -> Dict[str, Any] | None:
    """Validate and return a normalized trusted policy for this allocation."""

    spec = pool.get("spec") or {}
    template = spec.get("template") or {}
    template_meta = template.get("metadata") or {}
    annotations = template_meta.get("annotations") or {}
    capability = annotations.get(EXECUTION_ISOLATION_ANNOTATION)

    if capability != BWRAP_V1:
        if isolation is not None:
            raise PoolIsolationError(
                "isolation is accepted only by a Pool advertising "
                f"{EXECUTION_ISOLATION_ANNOTATION}={BWRAP_V1}"
            )
        return None
    if isolation is None:
        raise PoolIsolationError("this Pool requires isolation.type=bwrap")
    if isolation.type != "bwrap":
        raise PoolIsolationError("this Pool supports only isolation.type=bwrap")
    if (spec.get("recycleStrategy") or {}).get("type", "Delete") != "Delete":
        raise PoolIsolationError("bwrap-v1 Pool requires recycleStrategy.type=Delete")

    raw_policy = annotations.get(BWRAP_MOUNT_POLICY_ANNOTATION)
    if not isinstance(raw_policy, str) or not raw_policy.strip():
        raise PoolIsolationError("bwrap-v1 Pool is missing its mount policy annotation")
    try:
        policy = json.loads(raw_policy)
    except (TypeError, json.JSONDecodeError) as exc:
        raise PoolIsolationError("bwrap-v1 Pool mount policy is not valid JSON") from exc
    if not isinstance(policy, dict) or policy.get("version") != 1:
        raise PoolIsolationError("bwrap-v1 Pool mount policy version must be 1")
    roots = policy.get("roots")
    if not isinstance(roots, dict) or not roots:
        raise PoolIsolationError("bwrap-v1 Pool mount policy roots must not be empty")
    policy_mount_roots = {
        raw_root.get("mountRoot")
        for raw_root in roots.values()
        if isinstance(raw_root, dict) and isinstance(raw_root.get("mountRoot"), str)
    }

    template_spec = template.get("spec") or {}
    if template_spec.get("automountServiceAccountToken") is not False:
        raise PoolIsolationError("bwrap-v1 Pool must set automountServiceAccountToken=false")
    container = _sandbox_container(template_spec)

    mounts = container.get("volumeMounts") or []
    volumes = template_spec.get("volumes") or []
    for volume in volumes:
        if not isinstance(volume, dict):
            continue
        if volume.get("hostPath") is not None:
            raise PoolIsolationError("bwrap-v1 Pool must not mount hostPath volumes")
        if volume.get("serviceAccountToken") is not None:
            raise PoolIsolationError("bwrap-v1 Pool must not mount ServiceAccount tokens")
        projected = volume.get("projected")
        if isinstance(projected, dict):
            for source in projected.get("sources") or []:
                if isinstance(source, dict) and source.get("serviceAccountToken") is not None:
                    raise PoolIsolationError(
                        "bwrap-v1 Pool must not mount projected ServiceAccount tokens"
                    )
    volume_by_name = {
        item.get("name"): item for item in volumes if isinstance(item, dict) and item.get("name")
    }
    mount_paths: Dict[str, list[str]] = {}
    for mount in mounts:
        if not isinstance(mount, dict):
            continue
        name, path = mount.get("name"), mount.get("mountPath")
        if isinstance(name, str) and isinstance(path, str):
            mount_paths.setdefault(name, []).append(path)

    normalized_roots: Dict[str, Dict[str, Any]] = {}
    declared_mount_roots: set[str] = set()
    for name, raw_root in roots.items():
        if not isinstance(name, str) or not name or not isinstance(raw_root, dict):
            raise PoolIsolationError("bwrap-v1 policy contains an invalid root")
        mount_root = raw_root.get("mountRoot")
        source = raw_root.get("source")
        prefixes = raw_root.get("targetPrefixes")
        max_mode = raw_root.get("maxMode")
        if not _absolute_clean(mount_root) or not _absolute_clean(source):
            raise PoolIsolationError(f"root {name!r} paths must be normalized absolute paths")
        if not _within(mount_root, source):
            raise PoolIsolationError(f"root {name!r} source must be beneath mountRoot")
        if mount_root == "/" or any(
            _within(control, mount_root) or _within(mount_root, control)
            for control in _CONTROL_TARGETS
        ):
            raise PoolIsolationError(f"root {name!r} mountRoot overlaps a control directory")
        if max_mode not in {"ro", "rw"}:
            raise PoolIsolationError(f"root {name!r} maxMode must be ro or rw")
        if (
            not isinstance(prefixes, list)
            or not prefixes
            or any(not _absolute_clean(prefix) or prefix == "/" for prefix in prefixes)
        ):
            raise PoolIsolationError(f"root {name!r} targetPrefixes are invalid")
        matching = [
            volume_name
            for volume_name, paths in mount_paths.items()
            if mount_root in paths
            and isinstance(volume_by_name.get(volume_name, {}).get("persistentVolumeClaim"), dict)
        ]
        if len(matching) != 1:
            raise PoolIsolationError(
                f"root {name!r} mountRoot must match exactly one PVC VolumeMount"
            )
        volume_name = matching[0]
        claim_name = volume_by_name[volume_name]["persistentVolumeClaim"].get("claimName")
        same_claim_mounts = [
            path
            for candidate_name, paths in mount_paths.items()
            if volume_by_name.get(candidate_name, {})
            .get("persistentVolumeClaim", {})
            .get("claimName")
            == claim_name
            for path in paths
        ]
        if any(path not in policy_mount_roots for path in same_claim_mounts):
            raise PoolIsolationError(
                f"PVC {claim_name!r} must not be mounted through an undeclared path"
            )
        declared_mount_roots.add(mount_root)
        normalized_roots[name] = {
            "mountRoot": mount_root,
            "source": source,
            "targetPrefixes": prefixes,
            "maxMode": max_mode,
        }

    targets: list[str] = []
    normalized_mounts: list[Dict[str, str]] = []
    for selector in isolation.mounts:
        root = normalized_roots.get(selector.root)
        if root is None:
            raise PoolIsolationError(f"unknown isolation root {selector.root!r}")
        if not any(_within(prefix, selector.target) for prefix in root["targetPrefixes"]):
            raise PoolIsolationError(
                f"target {selector.target!r} is outside root {selector.root!r} targetPrefixes"
            )
        if any(
            _within(control, selector.target) or _within(selector.target, control)
            for control in _CONTROL_TARGETS
        ):
            raise PoolIsolationError(f"target {selector.target!r} overlaps a control directory")
        if any(
            _within(root_path, selector.target) or _within(selector.target, root_path)
            for root_path in declared_mount_roots
        ):
            raise PoolIsolationError(f"target {selector.target!r} overlaps a trusted mount root")
        if root["maxMode"] == "ro" and selector.mode == "rw":
            raise PoolIsolationError(f"root {selector.root!r} does not allow rw mounts")
        if any(
            _within(target, selector.target) or _within(selector.target, target)
            for target in targets
        ):
            raise PoolIsolationError(f"target {selector.target!r} overlaps another mount target")
        targets.append(selector.target)
        normalized_mounts.append(selector.model_dump(by_alias=True))

    return {"type": "bwrap", "roots": normalized_roots, "mounts": normalized_mounts}
