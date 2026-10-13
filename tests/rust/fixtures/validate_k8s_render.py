# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0
"""Validate owned Helm renders. Uses the server environment's existing PyYAML."""
import sys
try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib
from pathlib import Path
import yaml

root = Path(sys.argv[1])

def documents(name):
    return [d for d in yaml.safe_load_all((root / name).read_text()) if d]

def deployment(name):
    return next(d for d in documents(name) if d["kind"] == "Deployment")

controller = deployment("controller.yaml")["spec"]["template"]["spec"]["containers"][0]
assert controller["image"] == "opensandbox/controller:render-check"
for flag in ["--image-committer-image=opensandbox/image-committer:render-check", "--containerd-socket-path=/run/containerd/containerd.sock", "--snapshot-registry=127.0.0.1:30500/rust-snapshots", "--snapshot-registry-insecure=true"]:
    assert flag in controller["args"], flag
server_docs = documents("server.yaml")
config = next(d["data"]["config.toml"] for d in server_docs if d["kind"] == "ConfigMap")
toml = tomllib.loads(config)
assert toml["runtime"]["type"] == "kubernetes"
assert toml["kubernetes"]["workload_provider"] == "batchsandbox"
assert toml["ingress"]["mode"] == "gateway"
assert toml["ingress"]["gateway"]["route"]["mode"] == "uri"
ring = toml["ingress"]["secure_access"]
assert ring["active_key"] == "a" and len(ring["keys"]) == 1
key = ring["keys"][0]["key"]
assert len(key) >= 32
server = deployment("server.yaml")["spec"]["template"]["spec"]
assert server["containers"][0]["image"] == "opensandbox/server:render-check"
assert any(m["mountPath"] == "/etc/opensandbox/e2e.batchsandbox-template.yaml" for m in server["containers"][0]["volumeMounts"])
gateway = deployment("gateway.yaml")["spec"]["template"]["spec"]["containers"][0]
assert gateway["image"] == "opensandbox/ingress:render-check"
assert f"--secure-access-keys=a={key}" in gateway["args"]
assert any("uri" in arg for arg in gateway["args"])
print("Helm controller flags/images, server TOML/template mount and matching gateway signing ring verified")
