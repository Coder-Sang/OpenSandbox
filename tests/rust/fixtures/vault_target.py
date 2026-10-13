# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0
"""Only safe synthetic credentials. Never echo request headers or credentials."""
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Target(BaseHTTPRequestHandler):
    def do_GET(self):
        healthy = self.path == "/healthz"
        authorized = self.headers.get("Authorization") == "Bearer rust-e2e-safe-token"
        self.send_response(200 if healthy or authorized else 401)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(json.dumps({"ok": healthy or authorized}).encode())

    def log_message(self, *args):
        pass


ThreadingHTTPServer(("0.0.0.0", 80), Target).serve_forever()
