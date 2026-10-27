#!/usr/bin/env python3
"""Fail-closed source RC gate; does not publish or qualify binary artifacts."""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re


REQUIRED_JOBS = {"rc-request", "rc-checks", "rc-hygiene", "rc-security", "rc-fuzz"}


def request_context(event: dict, env: dict) -> dict:
    repository = event.get("repository", {})
    inputs = event.get("inputs", {})
    branch = repository.get("default_branch")
    sha = env.get("GITHUB_SHA", "")
    if (env.get("GITHUB_EVENT_NAME") != "workflow_dispatch"
            or repository.get("private") is not False
            or not branch or env.get("GITHUB_REF") != f"refs/heads/{branch}"
            or inputs.get("channel") != "rc"
            or not repository.get("full_name")
            or repository["full_name"] != env.get("GITHUB_REPOSITORY")
            or not re.fullmatch(r"[0-9a-f]{40}", sha)):
        raise ValueError("RC requires a public repository, default branch and channel=rc")
    expected = inputs.get("expected_sha", "")
    if expected and expected != sha:
        raise ValueError("Default branch moved: expected_sha differs from this run")
    run_id = int(env["GITHUB_RUN_ID"])
    attempt = int(env["GITHUB_RUN_ATTEMPT"])
    if run_id < 1 or attempt < 1:
        raise ValueError("Invalid run identity")
    return {"repository": repository["full_name"], "source_sha": sha,
            "run_id": run_id, "run_attempt": attempt, "channel": "rc"}


def check_jobs(results: dict) -> None:
    if set(results) != REQUIRED_JOBS:
        raise ValueError("RC job inventory differs")
    failed = {name: job.get("result") for name, job in results.items()
              if job.get("result") != "success"}
    if failed:
        raise ValueError(f"RC blocked by unsuccessful or skipped stages: {failed}")


def check_report(report: dict, manifest_bytes: bytes, context: dict) -> dict:
    manifest = json.loads(manifest_bytes)
    manifest_digest = hashlib.sha256(manifest_bytes).hexdigest()
    expected = {**context, "schema": 3, "evidence_mode": "candidate",
                "private": False, "network": "isolated", "status": "passed",
                "manifest_sha256": manifest_digest}
    for key, value in expected.items():
        if type(report.get(key)) is not type(value) or report[key] != value:
            raise ValueError(f"Fuzz report does not match this RC: {key}")
    configured = manifest["targets"]
    targets = report.get("targets", [])
    names = [target["name"] for target in targets]
    expected_names = [target["name"] for target in configured]
    if (not names or len(names) != len(set(names))
            or len(expected_names) != len(set(expected_names))
            or set(names) != set(expected_names)):
        raise ValueError("Fuzz target inventory differs")
    by_name = {target["name"]: target for target in configured}
    for target in targets:
        config = by_name[target["name"]]
        for key in ("engine", "seconds"):
            if type(target.get(key)) is not type(config[key]) or target[key] != config[key]:
                raise ValueError(f"Fuzz target configuration differs: {target['name']}")
        if type(target.get("exit_code")) is not int or target["exit_code"] != 0:
            raise ValueError(f"Fuzz target failed: {target['name']}")
        if type(target.get("executions")) is not int or target["executions"] < 1:
            raise ValueError(f"Fuzz target did not execute: {target['name']}")
        elapsed = target.get("elapsed_seconds")
        if (type(elapsed) not in (int, float) or not math.isfinite(elapsed)
                or elapsed <= 0):
            raise ValueError(f"Fuzz target has no valid elapsed time: {target['name']}")
    return {"schema": 1, **context, "status": "passed", "evidence_mode": "source-rc",
            "manifest_sha256": manifest_digest,
            "fuzz_executions": sum(target["executions"] for target in targets),
            "targets": names, "required_stages": sorted(REQUIRED_JOBS),
            "scope": "Source RC; binary release and deployment acceptance are separate"}


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("request")
    commands.add_parser("jobs")
    report_parser = commands.add_parser("report")
    report_parser.add_argument("report", type=Path)
    report_parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        event = json.loads(Path(os.environ["GITHUB_EVENT_PATH"]).read_text())
        context = request_context(event, os.environ)
        if args.command == "jobs":
            check_jobs(json.loads(os.environ["RC_JOB_RESULTS"]))
        elif args.command == "report":
            raw_report = args.report.read_bytes()
            result = check_report(json.loads(raw_report), Path(".github/fuzz.json").read_bytes(), context)
            result["fuzz_report_sha256"] = hashlib.sha256(raw_report).hexdigest()
            args.output.write_text(json.dumps(result, indent=2) + "\n")
        print(f"RC {args.command}: passed")
    except (ValueError, OSError, KeyError, TypeError, AttributeError) as error:
        parser.exit(1, f"RC blocked: {error}\n")


if __name__ == "__main__":
    main()
