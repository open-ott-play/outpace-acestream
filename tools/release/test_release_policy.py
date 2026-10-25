import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

import release_policy as policy


def release(tag, *, draft=False, prerelease=None):
    return {"tag_name": tag, "draft": draft,
            "prerelease": "-beta." in tag if prerelease is None else prerelease}


class ReleasePolicyTests(unittest.TestCase):
    def test_only_canonical_stable_and_numbered_beta_tags_are_accepted(self):
        for tag in ("v0.0.0", "v1.20.3", "v0.9.0-beta.1", "v2.10.0-beta.123"):
            with self.subTest(tag=tag):
                policy.parse_tag(tag)
        for tag in ("1.2.3", "v01.2.3", "v1.02.3", "v1.2.03", "v1.2.3-beta",
                    "v1.2.3-beta.0", "v1.2.3-beta.01", "v1.2.3-beta.-1",
                    "v1.2.3-BETA.1", "v1.2.3-rc.1", "v1.2.3-alpha.1",
                    "v1.2.3+meta", "v1.2.3-beta.1+meta", "v1.2.3-beta.1.2",
                    "v1.2.3\n", "v1.2.3; echo no", "v1.2.3-beta.١", "--help"):
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                policy.parse_tag(tag)

    def test_metadata_is_bound_to_cargo_version_and_channel(self):
        self.assertEqual(policy.metadata("v0.9.0-beta.1", "0.9.0-beta.1"), {
            "version": "0.9.0-beta.1", "prerelease": "true",
            "make_latest": "false", "channel": "beta",
        })
        self.assertEqual(policy.metadata("v0.9.0", "0.9.0"), {
            "version": "0.9.0", "prerelease": "false",
            "make_latest": "true", "channel": "latest",
        })
        for cargo_version in ("0.9.0", "0.9.0-beta.2", "", "null"):
            with self.subTest(cargo_version=cargo_version), self.assertRaises(ValueError):
                policy.metadata("v0.9.0-beta.1", cargo_version)

    def test_published_and_draft_release_metadata_must_match_exactly(self):
        for tag in ("v0.9.0", "v0.9.0-beta.1"):
            for draft in (True, False):
                document = {"tagName": tag, "isDraft": draft, "isPrerelease": "beta" in tag}
                policy.validate_release(tag, document, draft=draft)
                for key, value in (("tagName", "v9.9.9"), ("isDraft", not draft),
                                   ("isDraft", int(draft)), ("isPrerelease", "beta" not in tag),
                                   ("isPrerelease", str(document["isPrerelease"]).lower())):
                    with self.subTest(tag=tag, draft=draft, key=key, value=value), self.assertRaises(ValueError):
                        policy.validate_release(tag, {**document, key: value}, draft=draft)
        for document in (None, [], {}, {"tagName": "v0.9.0"}):
            with self.assertRaises(ValueError):
                policy.validate_release("v0.9.0", document)

    def test_both_channels_can_create_or_resume_only_matching_unpublished_drafts(self):
        for tag in ("v0.9.0", "v0.9.0-beta.1"):
            with self.subTest(tag=tag):
                policy.ensure_unpublished(tag, [])
                policy.ensure_unpublished(tag, [release(tag, draft=True)])
                for document in ([release(tag)],
                                 [release(tag, draft=True, prerelease="beta" not in tag)]):
                    with self.assertRaises(ValueError):
                        policy.ensure_unpublished(tag, document)

    def test_publication_promotes_only_newest_stable_and_never_beta(self):
        for tag, other, expected in (
            ("v0.9.0", "v0.8.9", "true"),
            ("v0.10.0", "v0.9.9", "true"),
            ("v0.9.1", "v0.10.0", "false"),
            ("v1.2.3", "v2.0.0", "false"),
            ("v1.2.3", "v9.0.0-beta.1", "true"),
            ("v1.2.3-beta.1", "v0.1.0", "false"),
            ("v1.2.3-beta.2", "v1.2.3-beta.1", "false"),
        ):
            with self.subTest(tag=tag, other=other):
                self.assertEqual(policy.publication_metadata(
                    tag, [release(tag, draft=True), release(other)]),
                    {"make_latest": expected})
        self.assertEqual(policy.publication_metadata("v1.2.3", [
            release("v1.2.3", draft=True), release("v9.0.0", draft=True),
            release("legacy")]), {"make_latest": "true"})
        self.assertEqual(policy.publication_metadata(
            "v1.2.3-beta.1", [release("v1.2.3-beta.1", draft=True)]),
            {"make_latest": "false"})

    def test_publication_rechecks_matching_draft_and_rejects_inconsistent_inventory(self):
        for tag in ("v0.9.0", "v0.9.0-beta.1"):
            for document in ([], [release("v0.8.0")], [release(tag)],
                             [release(tag, draft=True, prerelease="beta" not in tag)],
                             [release(tag, draft=True), release(tag, draft=True)],
                             [release(tag, draft=True), release("v1.0.0", prerelease=True)],
                             [release(tag, draft=True), {"tag_name": "legacy"}]):
                with self.subTest(tag=tag, document=document), self.assertRaises(ValueError):
                    policy.publication_metadata(tag, document)

    def test_channels_compare_versions_numerically_and_independently(self):
        for tag, other in (("v0.9.0-beta.10", "v0.9.0-beta.2"),
                           ("v0.10.0-beta.1", "v0.9.99-beta.999"),
                           ("v1.0.0-beta.1", "v9.0.0"),
                           ("v0.10.0", "v0.9.0"),
                           ("v0.9.0", "v9.0.0-beta.1")):
            with self.subTest(tag=tag, other=other):
                policy.check_channel_advance(tag, [release(tag), release(other)])
        for tag, newer in (("v0.9.0-beta.2", "v0.9.0-beta.10"),
                           ("v0.9.0-beta.99", "v0.10.0-beta.1"),
                           ("v0.9.0", "v0.10.0")):
            with self.subTest(tag=tag, newer=newer), self.assertRaisesRegex(ValueError, "newer"):
                policy.check_channel_advance(tag, [release(tag), release(newer)])

    def test_channel_allows_equal_version_but_not_missing_or_unpublished_release(self):
        for tag in ("v0.9.0", "v0.9.0-beta.1"):
            policy.check_channel_advance(tag, [release(tag)])
            policy.check_channel_advance(tag, [release(tag), release("v9.9.9-beta.1", draft=True),
                                               release("v9.9.9", draft=True), release("legacy")])
            for document in ([], [release(tag, draft=True)], [release("v0.8.0")]):
                with self.subTest(tag=tag, document=document), self.assertRaisesRegex(ValueError, "still be published"):
                    policy.check_channel_advance(tag, document)

    def test_channel_fails_closed_on_malformed_or_inconsistent_metadata(self):
        tag = "v0.9.0-beta.1"
        for document in (None, {}, [None], [{}], [[release(tag)]], [{"tag_name": tag}],
                         [release(tag), {"tag_name": "legacy", "draft": "false", "prerelease": False}],
                         [release(tag, prerelease=False)], [release(tag), release("v1.0.0", prerelease=True)],
                         [release(tag), release(tag)]):
            with self.subTest(document=document), self.assertRaises(ValueError):
                policy.check_channel_advance(tag, document)

    def test_cli_emits_action_outputs_and_fails_before_any_output_on_invalid_tag(self):
        script = str(Path(policy.__file__))
        completed = subprocess.run([sys.executable, script, "metadata", "v0.9.0-beta.1",
                                    "--cargo-version", "0.9.0-beta.1"], text=True, capture_output=True)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        self.assertEqual(completed.stdout, "version=0.9.0-beta.1\nprerelease=true\nmake_latest=false\nchannel=beta\n")
        completed = subprocess.run([sys.executable, script, "metadata", "v0.9.0-rc.1"], text=True, capture_output=True)
        self.assertNotEqual(completed.returncode, 0)
        self.assertEqual(completed.stdout, "")
        with tempfile.TemporaryDirectory() as tmp:
            document = Path(tmp) / "releases.json"
            document.write_text(json.dumps([release("v0.9.0-beta.1"), release("v0.9.0-beta.2")]))
            completed = subprocess.run([sys.executable, script, "channel", "v0.9.0-beta.1", str(document)],
                                       text=True, capture_output=True)
            self.assertNotEqual(completed.returncode, 0)
            self.assertIn("newer beta", completed.stderr)
            document.write_text(json.dumps([release("v0.9.1", draft=True), release("v0.10.0")]))
            completed = subprocess.run([sys.executable, script, "publication", "v0.9.1", str(document)],
                                       text=True, capture_output=True)
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(completed.stdout, "make_latest=false\n")
            document.write_text(json.dumps([release("v0.9.1")]))
            completed = subprocess.run([sys.executable, script, "publication", "v0.9.1", str(document)],
                                       text=True, capture_output=True)
            self.assertNotEqual(completed.returncode, 0)
            self.assertEqual(completed.stdout, "")


if __name__ == "__main__":
    unittest.main()
