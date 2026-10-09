"""Run with python3 -m unittest discover -s scripts -p 'test_*.py'."""
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import yaml

import generate_api as gen


class GeneratorTests(unittest.TestCase):
    def test_real_specs_complete_and_deterministic(self):
        first = gen.outputs(gen.ROOT / "specs")
        self.assertEqual(first, gen.outputs(gen.ROOT / "specs"))
        for module, filename in gen.SPECS.items():
            spec = yaml.load((gen.ROOT / "specs" / filename).read_text(), Loader=gen.SpecLoader)
            result = first[module + ".rs"]
            for name in spec["components"]["schemas"]:
                self.assertRegex(result, r"pub (?:struct|enum|type) " + name + r"\b")
            for path, item in spec["paths"].items():
                for method, op in item.items():
                    if method in {"get", "put", "post", "delete", "patch", "head", "options", "trace"}:
                        operation_id = op.get("operationId", gen.snake(method + "_" + path))
                        self.assertIn('id: "' + operation_id + '"', result)
            self.assertIn("pub const OPERATIONS", result)
            self.assertIn("pub const SCHEMAS", result)

    def test_nested_enum_nullable_map_and_binary(self):
        schema = {"type": "object", "required": ["wireKey"], "properties": {
            "wireKey": {"type": "string"},
            "type": {"type": "string", "enum": ["first-value", "second"]},
            "nullable": {"oneOf": [{"type": "integer"}, {"type": "null"}]},
            "map": {"type": "object", "additionalProperties": {"type": "array", "items": {"type": "string"}}},
            "blob": {"type": "string", "format": "binary"},
        }, "additionalProperties": False}
        generator = gen.Generator({"components": {"schemas": {"Test": schema}}})
        output = generator.generate()
        self.assertIn('rename = "wireKey"', output)
        self.assertIn("pub wire_key: String", output)
        self.assertIn("pub type_: Option<TestType>", output)
        self.assertIn("pub nullable: Option<Option<i64>>", output)
        self.assertIn("deserialize_present_nullable", output)
        self.assertIn("BTreeMap<String, Vec<String>>", output)
        self.assertIn("pub blob: Option<Vec<u8>>", output)
        self.assertIn('Self::FirstValue => "first-value"', output)

    def test_oneof_exact_match_and_allof(self):
        schemas = {"A": {"type": "string"}, "B": {"type": "integer"},
                   "Choice": {"oneOf": [{"$ref": "#/components/schemas/A"}, {"$ref": "#/components/schemas/B"}]},
                   "Combined": {"type": "object", "allOf": [{"type": "object", "required": ["id"], "properties": {"id": {"type": "string"}}}], "properties": {"count": {"type": "integer"}}}}
        output = gen.Generator({"components": {"schemas": schemas}}).generate()
        self.assertIn("count == 1", output)
        self.assertIn("pub id: String", output)
        self.assertIn("pub count: Option<i64>", output)

    def test_yaml_keeps_wire_scalars(self):
        result = yaml.load("timestamp: 2026-10-09T00:00:00Z\nvalue: no\nflag: false\n", Loader=gen.SpecLoader)
        self.assertIsInstance(result["timestamp"], str)
        self.assertEqual(result["value"], "no")
        self.assertIs(result["flag"], False)

    def test_collision_and_unknown_type_fail(self):
        for properties in ({"wireKey": {"type": "string"}, "wire_key": {"type": "string"}}, {"bad": {"type": "unknown"}}):
            with self.assertRaises(ValueError):
                gen.Generator({"components": {"schemas": {"Test": {"type": "object", "properties": properties}}}}).generate()

    def test_multipart_binary_and_parameters_metadata(self):
        result = gen.outputs(gen.ROOT / "specs")["execd.rs"]
        self.assertIn('content_type: "multipart/form-data"', result)
        self.assertIn('content_type: "application/octet-stream"', result)
        self.assertIn('content_type: "text/event-stream"', result)
        self.assertIn("pub file: Option<Vec<u8>>", result)
        self.assertIn('"contentType":"application/json"', result)
        self.assertIn('location: "query"', result)

    def test_diagnostics_contract_is_generated(self):
        result = gen.outputs(gen.ROOT / "specs")
        self.assertIn("pub mod diagnostic;", result["mod.rs"])
        diagnostic = result["diagnostic.rs"]
        self.assertIn("pub struct DiagnosticContentResponse", diagnostic)
        self.assertIn('path: "/sandboxes/{sandboxId}/diagnostics/logs"', diagnostic)
        self.assertIn('path: "/sandboxes/{sandboxId}/diagnostics/events"', diagnostic)
        self.assertIn('name: "scope", location: "query", required: true', diagnostic)
        self.assertIn("pub content_url: Option<String>", diagnostic)
        self.assertIn("pub expires_at: Option<String>", diagnostic)

    def test_check_does_not_write(self):
        with tempfile.TemporaryDirectory() as directory:
            target = Path(directory) / "does-not-exist"
            result = subprocess.run([sys.executable, str(Path(gen.__file__)), "--check", "--output-dir", str(target)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 1, result.stderr)
            self.assertFalse(target.exists())
            self.assertIn("Stale generated files", result.stderr)


if __name__ == "__main__":
    unittest.main()
