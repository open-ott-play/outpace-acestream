import json
from pathlib import Path
import subprocess
import tempfile
import unittest
from prepare_context import prepare


class ContextTest(unittest.TestCase):
    def test_actual_tracked_bytes_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'repo'
            root.mkdir()
            subprocess.run(['git', 'init', '--quiet', str(root)], check=True)
            (root / 'source.rs').write_text('original')
            subprocess.run(['git', '-C', str(root), 'add', '.'], check=True)
            subprocess.run(['git', '-C', str(root), '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid', '-c', 'core.hooksPath=missing-hooks', 'commit', '--quiet', '-m', 'initial'], check=True)
            (root / 'source.rs').write_text('modified')
            (root / 'private.env').write_text('DO_NOT_PUBLISH')
            output = Path(tmp) / 'context'
            prepare(root, output)
            self.assertEqual((output / 'source.rs').read_text(), 'modified')
            self.assertFalse((output / 'private.env').exists())
            self.assertFalse((output / '.git').exists())
            info = json.loads((output / 'SOURCE-MANIFEST.json').read_text())
            self.assertEqual(info['tree_state'], 'modified')
            with self.assertRaises(FileExistsError):
                prepare(root, output)

    def test_staged_environment_file_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / 'repo'
            root.mkdir()
            subprocess.run(['git', 'init', '--quiet', str(root)], check=True)
            (root / '.env').write_text('DO_NOT_PUBLISH')
            subprocess.run(['git', '-C', str(root), 'add', '.'], check=True)
            output = Path(tmp) / 'context'
            with self.assertRaises(ValueError):
                prepare(root, output)
            self.assertFalse(output.exists())
