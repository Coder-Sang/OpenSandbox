# Copyright 2026 The OpenSandbox Authors
# SPDX-License-Identifier: Apache-2.0
"""Regression checks for Rust E2E selection and CI diagnostic retention."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[4]
RUNNER = ROOT / "scripts/rust-e2e.sh"


class RustE2ETests(unittest.TestCase):
    def run_runner(self, **settings):
        # Exercise the actual selector without Docker, Rust or remote resources.
        source = RUNNER.read_text().split('if [[ -n "${OPENSANDBOX_TEST_DOMAIN:-}"', 1)[0]
        with tempfile.TemporaryDirectory() as directory:
            scratch = Path(directory)
            cargo = scratch / "cargo"
            cargo.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['CALLS'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "if '--list' in sys.argv:\n"
                "    print('docker_interpreter_state: test')\n"
                "    print('docker_identity_files_permissions_ownership_and_all_routes: test')\n"
                "    print('docker_native_argv_literal_options_stream_background_failure_interrupt: test')\n"
                "elif os.environ.get('FAIL_SUITE') in sys.argv:\n"
                "    sys.exit(1)\n"
            )
            cargo.chmod(0o700)
            env = os.environ.copy()
            for key in (
                "RUN_CODE_INTERPRETER_E2E",
                "OPENSANDBOX_RUST_INTERPRETER_ONLY",
                "OPENSANDBOX_RUST_INTERPRETER_FILTER",
                "OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY",
            ):
                env.pop(key, None)
            env.update(settings)
            env.update(
                PATH=f"{scratch}:{env['PATH']}",
                CALLS=str(scratch / "calls.jsonl"),
                JCODE_SCRATCH_DIR=directory,
            )
            result = subprocess.run(
                ["bash", "-c", source + '\nrun_scenarios\nexit "$FAILED"', str(RUNNER)],
                env=env,
                capture_output=True,
                text=True,
                check=False,
            )
            calls = [json.loads(line) for line in (scratch / "calls.jsonl").read_text().splitlines()]
            return result.returncode, calls

    def test_default_excludes_only_interpreter_in_expanded_suite(self):
        code, calls = self.run_runner()
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 10)
        expanded = [call for call in calls if "expanded" in call]
        self.assertEqual(len(expanded), 2)
        for call in expanded:
            self.assertEqual(call[-2:], ["--skip", "docker_interpreter"])
        for suite in ("overlays", "identity_argv", "sandbox_create_command_files_metrics_egress_kill"):
            self.assertEqual(sum(suite in call for call in calls), 2)

    def test_enabled_interpreter_runs_unfiltered_expanded_suite(self):
        code, calls = self.run_runner(RUN_CODE_INTERPRETER_E2E="true")
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 10)
        self.assertTrue(all("--skip" not in call for call in calls))

    def test_explicit_false_matches_default(self):
        self.assertEqual(self.run_runner(), self.run_runner(RUN_CODE_INTERPRETER_E2E="false"))

    def test_focused_interpreter_is_explicit_opt_in(self):
        code, calls = self.run_runner(OPENSANDBOX_RUST_INTERPRETER_ONLY="true")
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 3)
        self.assertTrue(all("docker_interpreter" in call for call in calls))
        self.assertTrue(all("--skip" not in call for call in calls))

    def test_focused_identity_is_unchanged(self):
        code, calls = self.run_runner(OPENSANDBOX_RUST_IDENTITY_ARGV_ONLY="true")
        self.assertEqual(code, 0)
        self.assertEqual(len(calls), 3)
        self.assertTrue(all("identity_argv" in call for call in calls))

    def test_failure_still_fails_runner_and_runs_both_modes(self):
        code, calls = self.run_runner(FAIL_SUITE="expanded")
        self.assertEqual(code, 1)
        self.assertEqual(len(calls), 10)

    def test_failed_prerequisite_fails_both_modes_before_main_scenarios(self):
        code, calls = self.run_runner(FAIL_SUITE="prerequisites")
        self.assertEqual(code, 1)
        self.assertEqual(len(calls), 2)
        self.assertTrue(all("prerequisites" in call for call in calls))

    def test_cleanup_preserves_fatal_startup_failure(self):
        source = RUNNER.read_text()
        cleanup = source[source.index('SERVER_PID=""'):source.index('export OPENSANDBOX_TEST_PORT=')]
        with tempfile.TemporaryDirectory() as directory:
            scratch = Path(directory)
            docker = scratch / "docker"
            docker.write_text("#!/bin/sh\nexit 0\n")
            docker.chmod(0o700)
            env = os.environ.copy()
            env.pop("RUST_E2E_TEST_UNSET", None)
            env.update(PATH=f"{scratch}:{env['PATH']}", RUN_DIR=directory)
            for command, expected in (
                ('printf "%s" "$RUST_E2E_TEST_UNSET"', 1),
                ("exit 19", 19),
                ("RUN_FINISHED=true; exit 0", 0),
            ):
                with self.subTest(command=command):
                    result = subprocess.run(
                        ["bash", "-c", 'set -euo pipefail\n' + cleanup + '\n' + command],
                        env=env,
                        capture_output=True,
                        text=True,
                        check=False,
                    )
                    self.assertEqual(result.returncode, expected)
                    if "RUST_E2E_TEST_UNSET" in command:
                        self.assertIn("unbound variable", result.stderr)

    def test_workflow_uses_shared_interpreter_switch_and_uploads_traces(self):
        workflow = yaml.load(
            (ROOT / ".github/workflows/real-e2e.yml").read_text(), Loader=yaml.BaseLoader
        )
        jobs = workflow["jobs"]
        self.assertEqual(
            jobs["rust-e2e"]["env"]["RUN_CODE_INTERPRETER_E2E"],
            jobs["python-e2e"]["env"]["RUN_CODE_INTERPRETER_E2E"],
        )
        upload = next(step for step in jobs["rust-e2e"]["steps"] if "upload-artifact" in step.get("uses", ""))
        self.assertEqual(upload["if"], "always()")
        self.assertIn("${{ runner.temp }}/opensandbox-rust-e2e.*/interpreter/*.json", upload["with"]["path"].splitlines())
        self.assertIn("${{ runner.temp }}/opensandbox-rust-e2e.*/prerequisites-*.txt", upload["with"]["path"].splitlines())
        self.assertNotIn("${{ runner.temp }}/opensandbox-rust-e2e.*/**", upload["with"]["path"].splitlines())


if __name__ == "__main__":
    unittest.main()
