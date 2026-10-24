import hashlib
import json
from pathlib import Path
import tempfile
import unittest
from package_source import package


class PackageSourceTests(unittest.TestCase):
    def test_uses_this_build_output_and_verifies_digest(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            output = root / 'build-output'
            output.mkdir()
            source = b'corresponding source fixture'
            (output / 'corresponding-source.tar.gz').write_bytes(source)
            (output / 'source.json').write_text(json.dumps({'project': 'Outpace AceStream', 'source_sha256': hashlib.sha256(source).hexdigest()}))
            events = root / 'cargo.json'
            events.write_text(json.dumps({'reason': 'build-script-executed', 'out_dir': str(output)}) + '\n')
            destination = root / 'package'
            destination.mkdir()
            package(events, destination)
            self.assertEqual((destination / 'corresponding-source.tar.gz').read_bytes(), source)
            (output / 'corresponding-source.tar.gz').write_bytes(b'wrong source')
            with self.assertRaises(ValueError):
                package(events, destination)

    def test_no_source_output_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            events = Path(tmp) / 'cargo.json'
            events.write_text('{"reason":"compiler-artifact"}\n')
            with self.assertRaises(ValueError):
                package(events, Path(tmp))
