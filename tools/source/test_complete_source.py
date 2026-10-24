import hashlib
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from complete_source import prepare, dependencies


class CompleteSourceTests(unittest.TestCase):
    def fixture(self, root):
        checksum = 'a' * 64
        lock = f'[[package]]\nname="demo"\nversion="1.0.0"\nsource="registry+https://github.com/rust-lang/crates.io-index"\nchecksum="{checksum}"\n'.encode()
        package = root / 'package'
        package.mkdir()
        engine = package / 'corresponding-source.tar.gz'
        with tarfile.open(engine, 'w:gz') as archive:
            item = tarfile.TarInfo('outpace-acestream/Cargo.lock')
            item.size = len(lock)
            archive.addfile(item, io.BytesIO(lock))
        (package / 'source.json').write_text(json.dumps({'source_sha256': hashlib.sha256(engine.read_bytes()).hexdigest()}))
        vendor = root / 'vendor'
        crate = vendor / 'demo-1.0.0'
        crate.mkdir(parents=True)
        files = {'Cargo.toml': b'[package]\nname="demo"\nversion="1.0.0"\nlicense="MIT"', 'lib.rs': b'// source', 'LICENSE-MIT': b'Original license notice'}
        for name, data in files.items():
            (crate / name).write_bytes(data)
        (crate / '.cargo-checksum.json').write_text(json.dumps({'package': checksum, 'files': {name: hashlib.sha256(data).hexdigest() for name, data in files.items()}}))
        return package, vendor, lock

    def test_complete_source_contains_exact_workspace_dependencies_and_notices(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            package, vendor, _ = self.fixture(root)
            output = root / 'complete.tar.gz'
            self.assertEqual(prepare(package, vendor, output), 1)
            with tarfile.open(output) as archive:
                names = archive.getnames()
                self.assertIn('outpace-acestream-complete/vendor/demo-1.0.0/lib.rs', names)
                actual = archive.extractfile('outpace-acestream-complete/engine-source.tar.gz').read()
                self.assertEqual(actual, (package / 'corresponding-source.tar.gz').read_bytes())
            self.assertIn(b'Original license notice', (package / 'THIRD_PARTY_NOTICES.txt').read_bytes())
            first = output.read_bytes()
            prepare(package, vendor, output)
            self.assertEqual(first, output.read_bytes())

    def test_changed_or_extra_dependency_file_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            package, vendor, lock = self.fixture(Path(tmp))
            extra = vendor / 'demo-1.0.0/private.env'
            extra.write_text('PRIVATE_SENTINEL')
            with self.assertRaises(ValueError):
                dependencies(vendor, lock)
            extra.unlink()
            nested = vendor / 'demo-1.0.0/subdir/.cargo-checksum.json'
            nested.parent.mkdir()
            nested.write_text('PRIVATE_SENTINEL')
            with self.assertRaises(ValueError):
                dependencies(vendor, lock)
            nested.unlink()
            (vendor / 'demo-1.0.0/lib.rs').write_text('changed source')
            with self.assertRaises(ValueError):
                dependencies(vendor, lock)
