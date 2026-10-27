"""A skipped campaign or stale evidence must never qualify the current RC."""
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import rc_gate


class RcGateTests(unittest.TestCase):
    def setUp(self):
        self.event = {"repository": {"private": False, "default_branch": "main",
                                     "full_name": "example/engine"},
                      "inputs": {"channel": "rc"}}
        self.env = {"GITHUB_EVENT_NAME": "workflow_dispatch", "GITHUB_REF": "refs/heads/main",
                    "GITHUB_SHA": "1" * 40, "GITHUB_REPOSITORY": "example/engine",
                    "GITHUB_RUN_ID": "123", "GITHUB_RUN_ATTEMPT": "2"}
        self.context = rc_gate.request_context(self.event, self.env)
        self.manifest = json.dumps({"targets": [
            {"name": "parser", "engine": "libfuzzer", "seconds": 60}]}).encode()
        self.report = {**self.context, "schema": 3, "evidence_mode": "candidate",
                       "private": False, "network": "isolated", "status": "passed",
                       "manifest_sha256": hashlib.sha256(self.manifest).hexdigest(),
                       "targets": [{"name": "parser", "engine": "libfuzzer", "seconds": 60,
                                    "exit_code": 0, "executions": 17, "elapsed_seconds": 61.0}]}

    def test_public_default_branch_rc_and_expected_sha(self):
        self.event["inputs"]["expected_sha"] = self.env["GITHUB_SHA"]
        self.assertEqual(rc_gate.request_context(self.event, self.env), self.context)
        self.event["inputs"]["expected_sha"] = "2" * 40
        with self.assertRaisesRegex(ValueError, "moved"):
            rc_gate.request_context(self.event, self.env)

    def test_non_rc_events_branches_and_private_repositories_fail(self):
        for key, value in (("GITHUB_EVENT_NAME", "push"), ("GITHUB_EVENT_NAME", "pull_request"),
                           ("GITHUB_REF", "refs/tags/v1.0.0-rc.1"),
                           ("GITHUB_REF", "refs/heads/topic"), ("GITHUB_SHA", "bad"),
                           ("GITHUB_REPOSITORY", "different/repository"),
                           ("GITHUB_RUN_ATTEMPT", "0")):
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                rc_gate.request_context(self.event, {**self.env, key: value})
        for private in (True, None, "false", 0):
            event = copy.deepcopy(self.event)
            event["repository"]["private"] = private
            with self.subTest(private=private), self.assertRaises(ValueError):
                rc_gate.request_context(event, self.env)
        for channel in (None, "stable", "beta"):
            event = copy.deepcopy(self.event)
            event["inputs"]["channel"] = channel
            with self.subTest(channel=channel), self.assertRaises(ValueError):
                rc_gate.request_context(event, self.env)

    def test_every_stage_must_succeed(self):
        jobs = {name: {"result": "success"} for name in rc_gate.REQUIRED_JOBS}
        rc_gate.check_jobs(jobs)
        for name in jobs:
            for status in ("failure", "skipped", "cancelled", None):
                altered = copy.deepcopy(jobs)
                altered[name]["result"] = status
                with self.subTest(name=name, status=status), self.assertRaises(ValueError):
                    rc_gate.check_jobs(altered)
        with self.assertRaises(ValueError):
            rc_gate.check_jobs({name: job for name, job in jobs.items() if name != "rc-fuzz"})
        with self.assertRaises(ValueError):
            rc_gate.check_jobs({**jobs, "unexpected": {"result": "success"}})

    def test_success_is_bound_to_current_source_run_attempt_and_manifest(self):
        result = rc_gate.check_report(self.report, self.manifest, self.context)
        self.assertEqual(result["fuzz_executions"], 17)
        self.assertEqual(result["source_sha"], self.context["source_sha"])
        for key, value in (("schema", 2), ("run_id", 124), ("run_attempt", 1),
                           ("source_sha", "2" * 40), ("repository", "other/engine"),
                           ("channel", "stable"), ("evidence_mode", "release"),
                           ("private", 0), ("network", "unrestricted"),
                           ("status", "failed"), ("manifest_sha256", "0" * 64)):
            with self.subTest(key=key), self.assertRaises(ValueError):
                rc_gate.check_report({**self.report, key: value}, self.manifest, self.context)
        with self.assertRaises(ValueError):
            rc_gate.check_report(self.report, self.manifest + b"\n", self.context)

    def test_missing_duplicate_failed_and_unexecuted_targets_fail(self):
        target = self.report["targets"][0]
        for targets in ([], [target, target], [{**target, "name": "other"}]):
            with self.subTest(targets=targets), self.assertRaises(ValueError):
                rc_gate.check_report({**self.report, "targets": targets}, self.manifest, self.context)
        for key, value in (("engine", "go"), ("seconds", 1), ("seconds", "60"),
                           ("exit_code", 1), ("exit_code", False), ("executions", 0),
                           ("executions", True), ("elapsed_seconds", 0),
                           ("elapsed_seconds", float("nan")), ("elapsed_seconds", float("inf"))):
            altered = copy.deepcopy(self.report)
            altered["targets"][0][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                rc_gate.check_report(altered, self.manifest, self.context)

    def test_workflow_commands_reject_stale_evidence_without_writing_success(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / ".github").mkdir()
            (root / ".github/fuzz.json").write_bytes(self.manifest)
            (root / "event.json").write_text(json.dumps(self.event))
            (root / "report.json").write_text(json.dumps(self.report))
            env = {**os.environ, **self.env, "GITHUB_EVENT_PATH": str(root / "event.json"),
                   "RC_JOB_RESULTS": json.dumps({name: {"result": "success"}
                                                 for name in rc_gate.REQUIRED_JOBS})}
            command = [sys.executable, str(Path(rc_gate.__file__).resolve())]
            for arguments in (["request"], ["jobs"],
                              ["report", "report.json", "--output", "accepted.json"]):
                result = subprocess.run(command + arguments, env=env, cwd=root,
                                        text=True, capture_output=True)
                self.assertEqual(result.returncode, 0, result.stderr)
            accepted = json.loads((root / "accepted.json").read_text())
            self.assertEqual(accepted["fuzz_report_sha256"],
                             hashlib.sha256((root / "report.json").read_bytes()).hexdigest())
            result = subprocess.run(command + ["report", "report.json", "--output", "stale.json"],
                                    env={**env, "GITHUB_RUN_ATTEMPT": "3"}, cwd=root,
                                    text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("run_attempt", result.stderr)
            self.assertFalse((root / "stale.json").exists())


if __name__ == "__main__":
    unittest.main()
