"""Exercise the shipped helper against fixtures, never the host APT files."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[2]
AZURE = "http://azure.archive.ubuntu.com/ubuntu/\tpriority:1\n"
FALLBACK = "https://archive.ubuntu.com/ubuntu/\tpriority:2\n"


class UbuntuMirrorTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.mirrors = self.root / "apt-mirrors.txt"
        source = (ROOT / "tools/ci/prepare_ubuntu_mirrors.cjs").read_text()
        self.assertEqual(source.count("'/etc/apt/apt-mirrors.txt'"), 1)
        self.source = source.replace("'/etc/apt/apt-mirrors.txt'", json.dumps(str(self.mirrors)))

    def execute(self, **environment):
        return subprocess.run(
            ["node", "-e", self.source], capture_output=True, text=True,
            env=os.environ | {"RUNNER_ENVIRONMENT": "github-hosted", "RUNNER_OS": "Linux"} | environment,
            timeout=10, check=False,
        )

    def test_recognized_list_preserves_other_bytes_mode_and_second_run(self):
        retained = "# runner image\r\n" + FALLBACK + "https://security.ubuntu.com/ubuntu/ priority:3\n"
        self.mirrors.write_bytes((AZURE + retained).encode())
        self.mirrors.chmod(0o640)
        for _ in range(2):
            result = self.execute()
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.mirrors.read_bytes(), retained.encode())
            self.assertEqual(self.mirrors.stat().st_mode & 0o777, 0o640)

    def test_missing_custom_or_ambiguous_lists_are_unchanged(self):
        self.assertEqual(self.execute().returncode, 0)
        self.assertFalse(self.mirrors.exists())
        for content in (AZURE, AZURE * 2 + FALLBACK,
                        AZURE + FALLBACK.replace("https:", "http:"),
                        AZURE.replace("priority:1", "priority:9") + FALLBACK):
            with self.subTest(content=content):
                self.mirrors.write_text(content)
                result = self.execute()
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.mirrors.read_text(), content)

    def test_non_hosted_or_non_linux_does_not_access_the_list(self):
        self.mirrors.mkdir()
        for environment in ({"RUNNER_ENVIRONMENT": "self-hosted"}, {"RUNNER_OS": "macOS"}):
            self.assertEqual(self.execute(**environment).returncode, 0)
            self.assertTrue(self.mirrors.is_dir())

    def test_symlink_and_invalid_encoding_are_rejected_without_modification(self):
        target = self.root / "target"
        target.write_text(AZURE + FALLBACK)
        self.mirrors.symlink_to(target)
        self.assertNotEqual(self.execute().returncode, 0)
        self.assertEqual(target.read_text(), AZURE + FALLBACK)
        self.assertTrue(self.mirrors.is_symlink())
        self.mirrors.unlink()
        content = (AZURE + FALLBACK).encode() + b"\xff"
        self.mirrors.write_bytes(content)
        self.assertNotEqual(self.execute().returncode, 0)
        self.assertEqual(self.mirrors.read_bytes(), content)

    def test_both_ffmpeg_jobs_prepare_mirrors_before_installing(self):
        workflow = yaml.load((ROOT / ".github/workflows/ci.yml").read_text(), Loader=yaml.BaseLoader)
        for job, install_name in (("test", "Install ffmpeg"),
                                  ("hls-audio-synthetic", "Install synthetic media tools")):
            steps = workflow["jobs"][job]["steps"]
            guards = [step for step in steps if step.get("name") == "Prefer the provisioned Ubuntu archive fallback"]
            self.assertEqual(len(guards), 1)
            guard = guards[0]
            install = next(step for step in steps if step.get("name") == install_name)
            self.assertLess(steps.index(guard), steps.index(install))
            self.assertEqual(guard["if"], "${{ runner.environment == 'github-hosted' && runner.os == 'Linux' }}")
            self.assertIn("tools/ci/prepare_ubuntu_mirrors.cjs", guard["run"])
            self.assertNotIn("continue-on-error", guard)
            self.assertEqual(install["run"], "sudo apt-get update && sudo apt-get install -y --no-install-recommends ffmpeg")
            self.assertEqual(workflow["jobs"][job]["timeout-minutes"], "20")


if __name__ == "__main__":
    unittest.main()
