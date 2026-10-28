"""Regression checks for CI security and source-distribution boundaries.

BaseLoader preserves GitHub Actions' YAML 1.2 ``on`` spelling and reads scalar
values as strings. No workflow command is executed by these checks.
"""
from pathlib import Path
import json
import re
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
WORKFLOWS = ROOT / ".github" / "workflows"


def workflow(name):
    return yaml.load((WORKFLOWS / name).read_text(), Loader=yaml.BaseLoader)


def steps_text(job):
    return "\n".join(step.get("run", "") for step in job.get("steps", []))


class WorkflowContracts(unittest.TestCase):
    def test_required_checks_always_run_for_pull_requests(self):
        ci = workflow("ci.yml")
        hygiene = workflow("hygiene.yml")
        self.assertIn("pull_request", ci["on"])
        self.assertIn("pull_request", hygiene["on"])
        for document in (ci, hygiene):
            self.assertFalse(document["on"]["pull_request"])
        self.assertEqual(ci["on"]["push"], {"branches": ["main"]})
        expected = {
            "lint": "Rust lint", "test": "Rust test",
            "abcompare": "A/B harness test",
            "hls-audio-synthetic": "HLS audio synthetic",
        }
        for job_id, name in expected.items():
            job = ci["jobs"][job_id]
            self.assertEqual(job["name"], name)
            self.assertNotIn("if", job)
            self.assertNotIn("continue-on-error", job)
        identifier = hygiene["jobs"]["identifiers"]
        self.assertEqual(identifier["name"], "acestream identifier hygiene")
        self.assertNotIn("if", identifier)
        self.assertNotIn("continue-on-error", identifier)

    def test_pr_jobs_replay_seeds_without_mutational_fuzzing(self):
        jobs = workflow("ci.yml")["jobs"]
        self.assertIn("cargo fmt --manifest-path fuzz/Cargo.toml --all --check", steps_text(jobs["lint"]))
        self.assertIn("cargo clippy --manifest-path fuzz/Cargo.toml --all-targets --locked -- -D warnings", steps_text(jobs["lint"]))
        self.assertIn("cargo test --manifest-path fuzz/Cargo.toml --locked", steps_text(jobs["test"]))
        self.assertNotRegex("\n".join(steps_text(job) for job in jobs.values()), r"cargo(?:\s+\+\S+)?\s+fuzz\s+run")

    def test_origin_archive_decode_is_mandatory_with_generated_media_tools(self):
        steps = workflow("ci.yml")["jobs"]["hls-audio-synthetic"]["steps"]
        target = "origin_hls::tests::generated_origin_ts_archive_decodes_after_origin_removal"
        matching = [(index, step) for index, step in enumerate(steps)
                    if target in step.get("run", "")]
        self.assertEqual(len(matching), 1)
        index, step = matching[0]
        self.assertEqual(step["run"],
                         "cargo test -p ace-engine --lib --locked " + target
                         + " -- --ignored --exact --nocapture")
        self.assertNotIn("if", step)
        self.assertNotIn("continue-on-error", step)
        self.assertTrue(any("apt-get install" in previous.get("run", "")
                            and "ffmpeg" in previous["run"]
                            for previous in steps[:index]))
        source = (ROOT / "crates/ace-engine/src/origin_hls/tests.rs").read_text()
        self.assertIn("async fn " + target.rsplit("::", 1)[1] + "()", source)

    def test_remote_actions_are_immutable_and_vouched_for(self):
        allowed = {
            line.split()[0] for line in (ROOT / "tools/hygiene/allowed-identifiers.txt").read_text().splitlines()
            if line.strip() and not line.lstrip().startswith("#")
        }
        for path in WORKFLOWS.glob("*.yml"):
            for job in workflow(path.name)["jobs"].values():
                for step in [job, *job.get("steps", [])]:
                    ref = step.get("uses", "")
                    if not ref or ref.startswith("./"):
                        continue
                    with self.subTest(workflow=path.name, action=ref.split("@")[0]):
                        self.assertRegex(ref, r"^[^@]+@[0-9a-f]{40}$")
                        self.assertIn(ref.rsplit("@", 1)[1], allowed)
                        if ref.startswith("actions/checkout@"):
                            self.assertEqual(step["with"]["persist-credentials"], "false")

    def test_candidate_is_called_by_rc_pipeline_and_remains_read_only(self):
        doc = workflow("fuzz-candidate.yml")
        self.assertEqual(set(doc["on"]), {"workflow_call"})
        inputs = doc["on"]["workflow_call"]["inputs"]
        self.assertEqual(set(inputs), {"channel"})
        self.assertEqual(inputs["channel"]["type"], "string")
        self.assertEqual(inputs["channel"]["required"], "true")
        self.assertEqual(doc["permissions"], {"contents": "read"})
        self.assertEqual(set(doc["jobs"]), {"fuzz"})
        job = doc["jobs"]["fuzz"]
        guard = re.sub(r"\s+", " ", job["if"]).strip()
        self.assertEqual(guard, "github.event.repository.private == false && github.event_name == 'workflow_dispatch' && inputs.channel == 'rc' && github.ref == format('refs/heads/{0}', github.event.repository.default_branch)")
        self.assertNotIn("secrets", job)
        self.assertNotIn("continue-on-error", job)
        self.assertEqual(job["permissions"], {"contents": "read"})
        self.assertEqual(job["with"]["evidence-mode"], "candidate")
        self.assertNotIn("release-plan-artifact", job["with"])
        self.assertEqual(job["uses"].rsplit("@", 1)[1], job["with"]["toolkit-ref"])
        self.assertEqual(job["uses"].split("@")[0], "victron-venus/venus-os-ci-toolkit/.github/workflows/fuzz.yml")
        self.assertEqual(doc["concurrency"]["cancel-in-progress"], "false")

    def test_security_scan_is_public_and_separate_from_read_only_fuzz(self):
        doc = workflow("security.yml")
        self.assertEqual(set(doc["on"]), {"pull_request", "push", "workflow_call"})
        self.assertEqual(doc["on"]["push"], {"branches": ["main"]})
        self.assertEqual(doc["permissions"], {"contents": "read"})
        job = doc["jobs"]["security"]
        self.assertIn("${{ github.workflow }}", doc["concurrency"]["group"])
        self.assertEqual(job["if"], "github.event.repository.private == false")
        self.assertEqual(job["with"]["languages"], "rust,python,actions")
        self.assertNotIn("secrets", job)
        self.assertNotIn("continue-on-error", job)
        self.assertEqual(job["permissions"], {
            "contents": "read", "actions": "read", "security-events": "write"
        })
        candidate = workflow("fuzz-candidate.yml")["jobs"]["fuzz"]
        self.assertEqual(job["uses"].rsplit("@", 1)[1], candidate["with"]["toolkit-ref"])

    def test_toolkit_update_includes_references_and_reviewed_hash_inventory(self):
        package = "victron-venus/venus-os-ci-toolkit"
        expected = workflow("fuzz-candidate.yml")["jobs"]["fuzz"]["with"]["toolkit-ref"]
        for filename in ("fuzz-candidate.yml", "security.yml"):
            text = (WORKFLOWS / filename).read_text()
            self.assertRegex(text, re.escape(package) + r"/.+@" + expected + r" # main\n")
        config = json.loads((ROOT / "renovate.json").read_text())
        managers = [manager for manager in config["customManagers"]
                    if manager.get("customType") == "regex"
                    and any("allowed-identifiers" in path
                            for path in manager.get("managerFilePatterns", []))]
        self.assertEqual(len(managers), 1)
        manager = managers[0]
        self.assertEqual(manager["depNameTemplate"], package)
        self.assertEqual(manager["datasourceTemplate"], "git-refs")
        self.assertEqual(manager["currentValueTemplate"], "main")
        pattern = manager["matchStrings"][0].replace("(?<currentDigest>", "(?P<currentDigest>")
        inventory = (ROOT / "tools/hygiene/allowed-identifiers.txt").read_text()
        matches = list(re.finditer(pattern, inventory))
        self.assertEqual([match.group("currentDigest") for match in matches], [expected])
        for custom_manager in ("custom.jsonata", "custom.regex"):
            rules = [rule for rule in config["packageRules"]
                     if custom_manager in rule.get("matchManagers", [])
                     and package in rule.get("matchDepNames", [])]
            self.assertEqual(len(rules), 1)
            self.assertEqual(rules[0]["groupSlug"], "ci-workflows")
            self.assertFalse(rules[0]["automerge"])

    def test_stable_and_beta_release_keep_full_source_matrix_and_verifier(self):
        doc = workflow("release.yml")
        self.assertEqual(doc["on"]["push"], {"tags": ["v*.*.*", "!v*.*.*-*", "v*.*.*-beta.*"]})
        self.assertEqual(doc["jobs"]["release-metadata"]["if"], "github.event_name == 'push'")
        self.assertIn('tools/release/release_policy.py metadata "$GITHUB_REF_NAME"', steps_text(doc["jobs"]["release-metadata"]))
        self.assertIn('--cargo-version "$cargo_version"', steps_text(doc["jobs"]["release-metadata"]))
        build = doc["jobs"]["build-binaries"]
        targets = [row["target"] for row in build["strategy"]["matrix"]["include"]]
        self.assertEqual(set(targets), {
            "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl", "armv7-unknown-linux-musleabihf",
            "x86_64-apple-darwin", "aarch64-apple-darwin",
            "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
        })
        self.assertEqual(len(targets), 7)
        self.assertIn("tools/release/package_source.py", steps_text(build))
        self.assertIn("tools/source/complete_source.py", steps_text(build))
        publish = doc["jobs"]["publish-release"]
        self.assertIn("build-binaries", publish["needs"])
        verify_index = next(index for index, step in enumerate(publish["steps"]) if "tools/release/prepare_container.py" in step.get("run", ""))
        release_index = next(index for index, step in enumerate(publish["steps"]) if step.get("uses", "").startswith("softprops/action-gh-release@"))
        self.assertLess(verify_index, release_index)
        self.assertIn('"$GITHUB_SHA"', publish["steps"][verify_index]["run"])
        self.assertIn("SHA256SUMS", publish["steps"][release_index]["with"]["files"])
        self.assertEqual(doc["jobs"]["publish-container"]["needs"], "publish-release")

    def test_release_rc_automatically_requires_checks_and_fuzz_evidence(self):
        doc = workflow("release.yml")
        dispatch = doc["on"]["workflow_dispatch"]["inputs"]
        self.assertEqual(dispatch["channel"]["options"], ["rc"])
        self.assertEqual(dispatch["channel"]["required"], "true")
        jobs = doc["jobs"]
        self.assertEqual(jobs["rc-request"]["if"], "github.event_name == 'workflow_dispatch'")
        expected = {"rc-checks": "ci.yml", "rc-hygiene": "hygiene.yml",
                    "rc-security": "security.yml", "rc-fuzz": "fuzz-candidate.yml"}
        for name, filename in expected.items():
            job = jobs[name]
            self.assertEqual(job["needs"], "rc-request")
            self.assertEqual(job["uses"], f"./.github/workflows/{filename}")
            self.assertIn("workflow_call", workflow(filename)["on"])
            self.assertNotIn("continue-on-error", job)
            self.assertNotIn("if", job)
        self.assertEqual(jobs["rc-fuzz"]["with"], {"channel": "${{ inputs.channel }}"})
        gate = jobs["rc-gate"]
        self.assertEqual(set(gate["needs"]), {"rc-request", *expected})
        self.assertEqual(gate["if"], "always() && github.event_name == 'workflow_dispatch'")
        self.assertNotIn("continue-on-error", gate)
        self.assertIn("rc_gate.py jobs", steps_text(gate))
        self.assertIn("rc_gate.py report", steps_text(gate))
        download = next(step for step in gate["steps"]
                        if step.get("uses", "").startswith("actions/download-artifact@"))
        self.assertEqual(download["with"]["name"], "fuzz-candidate-${{ github.run_attempt }}")
        self.assertNotIn("run-id", download["with"])
        self.assertNotIn("github-token", download["with"])

    def test_all_release_assets_are_verified_in_draft_before_serial_publication(self):
        doc = workflow("release.yml")
        self.assertEqual(doc["concurrency"], {"group": "outpace-release-${{ github.ref }}", "cancel-in-progress": "false"})
        publish = doc["jobs"]["publish-release"]
        self.assertEqual(publish["concurrency"], {
            "group": "outpace-github-release-publication", "cancel-in-progress": "false"})
        steps = publish["steps"]
        upload_index = next(index for index, step in enumerate(steps) if step.get("uses", "").startswith("softprops/action-gh-release@"))
        upload = steps[upload_index]["with"]
        self.assertEqual(upload["prerelease"], "${{ needs.release-metadata.outputs.prerelease }}")
        self.assertEqual(upload["draft"], "true")
        self.assertEqual(upload["make_latest"], "false")
        self.assertEqual(upload["fail_on_unmatched_files"], "true")
        guard_index = next(index for index, step in enumerate(steps) if "release_policy.py unpublished" in step.get("run", ""))
        verify_index = next(index for index, step in enumerate(steps) if "release_policy.py draft" in step.get("run", ""))
        decision_index = next(index for index, step in enumerate(steps) if "release_policy.py publication" in step.get("run", ""))
        final_index = next(index for index, step in enumerate(steps) if "gh release edit" in step.get("run", ""))
        self.assertLess(guard_index, upload_index)
        self.assertLess(upload_index, verify_index)
        self.assertLess(verify_index, decision_index)
        self.assertLess(decision_index, final_index)
        for index in (guard_index, upload_index, verify_index, decision_index, final_index):
            self.assertNotIn("if", steps[index])
        self.assertIn("tools/release/prepare_container.py", steps[verify_index]["run"])
        decision = steps[decision_index]
        self.assertLess(decision["run"].index("gh api --paginate --slurp"),
                        decision["run"].index("release_policy.py publication"))
        self.assertEqual(steps[final_index]["env"]["MAKE_LATEST"],
                         "${{ steps." + decision["id"] + ".outputs.make_latest }}")
        self.assertEqual(steps[final_index]["env"]["PRERELEASE"],
                         "${{ needs.release-metadata.outputs.prerelease }}")
        final = steps[final_index]["run"]
        self.assertIn('--draft=false --prerelease="$PRERELEASE" --latest="$MAKE_LATEST" --verify-tag', final)
        self.assertIn("release_policy.py published", final)
        self.assertNotIn("make_latest", doc["jobs"]["release-metadata"]["outputs"])

    def test_paginated_release_lists_use_external_jq(self):
        for name in ("release.yml", "publish-container.yml"):
            commands = "\n".join(steps_text(job) for job in workflow(name)["jobs"].values())
            listings = [line for line in commands.splitlines() if "gh api --paginate --slurp" in line]
            self.assertEqual(len(listings), 2 if name == "release.yml" else 1)
            # gh --slurp cannot be combined with its built-in --jq option.
            for listing in listings:
                self.assertIn("| jq 'add'", listing)
                self.assertNotIn("--jq", listing)

    def test_containers_still_require_verified_assets_and_three_architectures(self):
        doc = workflow("publish-container.yml")
        jobs = doc["jobs"]
        self.assertIn("tools/release/prepare_container.py", steps_text(jobs["prepare"]))
        self.assertEqual(jobs["build"]["needs"], "prepare")
        arches = [row["arch"] for row in jobs["build"]["strategy"]["matrix"]["include"]]
        self.assertCountEqual(arches, ["amd64", "arm64", "arm-v7"])
        self.assertEqual(set(jobs["merge"]["needs"]), {"prepare", "build"})
        self.assertIn('"${#sources[@]}" != 3', steps_text(jobs["merge"]))
        self.assertEqual(doc["concurrency"]["cancel-in-progress"], "false")
        self.assertIn('release_policy.py published "$RELEASE_TAG"', steps_text(jobs["prepare"]))
        merge = jobs["merge"]
        self.assertEqual(merge["env"]["RELEASE_CHANNEL"], "${{ needs.prepare.outputs.channel }}")
        checkout = next(step for step in merge["steps"] if step.get("uses", "").startswith("actions/checkout@"))
        self.assertEqual(checkout["with"]["ref"], "${{ needs.prepare.outputs.tooling_revision }}")
        commands = steps_text(merge)
        self.assertIn("gh api --paginate --slurp", commands)
        self.assertLess(commands.index("release_policy.py channel"), commands.index("docker buildx imagetools create"))
        self.assertIn('--tag "${IMAGE_NAME}:${RELEASE_CHANNEL}"', commands)
        self.assertNotIn('--tag "${IMAGE_NAME}:latest"', commands)


if __name__ == "__main__":
    unittest.main()
