# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0

"""Operation generation must follow source changes and reject unsupported contracts."""
import copy
import unittest

import generate_api as gen
from generate_client import generate_client


def emit(spec):
    generator = gen.Generator(spec)
    models = generator.generate()
    definitions = set(generator.definitions)
    client = generate_client(generator, gen.snake, gen.pascal, gen.literal)
    if definitions != set(generator.definitions):
        raise AssertionError("Client introduced models missing from generated wire output")
    return models, client


def fixture():
    return {"components": {"schemas": {}, "parameters": {
        "Token": {"name": "X-Token", "in": "header", "required": True, "schema": {"type": "string"}},
    }}, "paths": {"/widgets/{sequence}": {
        "parameters": [{"name": "sequence", "in": "path", "required": True,
                        "schema": {"type": "integer", "minimum": 1, "maximum": 12}}],
        "post": {"operationId": "createWidget", "parameters": [
            {"name": "tags", "in": "query", "schema": {"type": "array", "items": {"type": "string"}}},
            {"name": "keys", "in": "query", "required": True, "explode": False,
             "schema": {"type": "array", "items": {"type": "string"}}},
            {"name": "enabled", "in": "query", "schema": {"type": "boolean"}},
            {"$ref": "#/components/parameters/Token"},
        ], "requestBody": {"required": True, "content": {"application/json": {"schema": {
            "type": "object", "required": ["label"], "properties": {"label": {"type": "string"}},
        }}}}, "responses": {"201": {"description": "Created", "content": {"application/json": {"schema": {
            "type": "object", "properties": {"id": {"type": "string"}},
        }}}}}},
    }}}


class ClientGeneratorTests(unittest.TestCase):
    def test_typed_paths_queries_headers_inline_models_and_bounds(self):
        models, client = emit(fixture())
        for token in ["pub sequence: u64", "pub tags: Option<&'a [String]>",
                      "pub keys: &'a [String]", "pub enabled: Option<bool>", "pub x_token: &'a str",
                      "(&params.sequence.to_string(), true)", "value < 1", "value > 12",
                      'append_pair("tags", item)', 'append_pair("keys", &value.join(","))',
                      'request.header("X-Token", params.x_token)', "request.json(body)",
                      "body: &CreateWidgetRequestBody",
                      "Call<CreateWidgetResponseValue201>"]:
            self.assertIn(token, client)
        self.assertIn("pub struct CreateWidgetRequestBody", models)

    def test_spec_changes_control_route_method_parameter_and_body(self):
        spec = fixture()
        original = emit(spec)[1]
        item = spec["paths"].pop("/widgets/{sequence}")
        spec["paths"]["/records/{sequence}"] = item
        operation = item.pop("post")
        item["patch"] = operation
        operation["parameters"][0]["name"] = "labels"
        operation["requestBody"]["required"] = False
        changed = emit(spec)[1]
        self.assertNotEqual(original, changed)
        self.assertIn('(\"records\", false)', changed)
        self.assertIn("reqwest::Method::PATCH", changed)
        self.assertIn('append_pair("labels", item)', changed)
        self.assertIn("Option<&CreateWidgetRequestBody>", changed)
        self.assertNotIn('(\"widgets\", false)', changed)

    def test_collisions_and_unsupported_serialization_fail_generation(self):
        for field, value in [("in", "cookie"), ("style", "deepObject")]:
            spec = fixture()
            spec["paths"]["/widgets/{sequence}"]["post"]["parameters"][0][field] = value
            with self.assertRaises(ValueError):
                emit(spec)
        spec = fixture()
        spec["paths"]["/other"] = {"get": {"operationId": "create_widget", "responses": {}}}
        with self.assertRaisesRegex(ValueError, "Colliding client operation"):
            emit(spec)
        spec = fixture()
        spec["paths"]["/widgets/{sequence}"]["post"]["parameters"].append(
            {"name": "x_token", "in": "query", "schema": {"type": "string"}})
        with self.assertRaisesRegex(ValueError, "Colliding client parameter"):
            emit(spec)

    def test_scoped_routes_are_read_from_spec_and_schema_drift_fails(self):
        spec = gen.yaml.load((gen.ROOT / "specs/execd-api.yaml").read_text(), Loader=gen.SpecLoader)
        original = emit(spec)[1]
        for path, item in list(spec["paths"].items()):
            if item.get("get", {}).get("operationId") == "isolatedGetFilesInfo":
                spec["paths"][path.replace("/files/info", "/files/inspect")] = spec["paths"].pop(path)
                operation = item["get"]
                break
        changed = emit(spec)[1]
        self.assertNotEqual(original, changed)
        scoped = changed[changed.index("fn get_files_info("):changed.index("fn download_file(")]
        self.assertIn('(\"inspect\", false)', scoped)
        operation["parameters"][0] = copy.deepcopy(operation["parameters"][0])
        operation["parameters"][0]["name"] = "different"
        with self.assertRaisesRegex(ValueError, "Scoped filesystem contract differs"):
            emit(spec)

    def test_missing_scoped_operation_cannot_generate_a_root_fallback(self):
        spec = gen.yaml.load((gen.ROOT / "specs/execd-api.yaml").read_text(), Loader=gen.SpecLoader)
        for path, item in list(spec["paths"].items()):
            if item.get("get", {}).get("operationId") == "isolatedGetFilesInfo":
                del spec["paths"][path]
                break
        with self.assertRaisesRegex(ValueError, "Missing scoped filesystem operation"):
            emit(spec)

    def test_identity_scope_absence_fails_closed(self):
        spec = gen.yaml.load((gen.ROOT / "specs/execd-api.yaml").read_text(), Loader=gen.SpecLoader)
        source = emit(spec)[1]
        self.assertIn("SUPPORTS_IDENTITY_FILES: bool = false", source)
        self.assertIn("Identity filesystem routes are unavailable", source)
        self.assertNotIn('("filesystem", false)', source)
