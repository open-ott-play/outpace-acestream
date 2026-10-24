import hashlib
import importlib.util
import io
import json
import zipfile
from pathlib import Path
import tarfile
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("prepare_container", Path(__file__).with_name("prepare_container.py"))
prepare = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prepare)


REVISION = "0123456789abcdef0123456789abcdef01234567"


class PrepareTests(unittest.TestCase):
    def fixture(self, root, member_type=tarfile.REGTYPE):
        for target, extension in prepare.TARGETS.items():
            archive = root / f"outpace-0.3.0-{target}.{extension}"
            prefix = f"outpace-0.3.0-{target}/"
            source = b"source archive fixture"
            values = {
                "corresponding-source.tar.gz": source,
                "source.json": json.dumps({"version": "0.3.0", "license": "AGPL-3.0-or-later", "project": "Outpace AceStream", "repository": "https://github.com/open-ott-play/outpace-acestream", "target": target, "base_revision": REVISION, "tree_state": "clean", "source_sha256": hashlib.sha256(source).hexdigest()}).encode(),
                "LICENSE": b"license", "NOTICE": b"notice", "AUTHORS.md": b"authors",
            }
            if extension == "tar.gz":
                with tarfile.open(archive, "w:gz") as tar:
                    member = tarfile.TarInfo(prefix + "outpace")
                    member.type = member_type
                    member.mode = 0o755
                    member.size = 3 if member_type == tarfile.REGTYPE else 0
                    tar.addfile(member, io.BytesIO(b"bin") if member.size else None)
                    for name, data in values.items():
                        member = tarfile.TarInfo(prefix + name)
                        member.size = len(data)
                        tar.addfile(member, io.BytesIO(data))
            else:
                with zipfile.ZipFile(archive, 'w') as target_zip:
                    for name, data in values.items():
                        target_zip.writestr(prefix + name, data)
        self.checksums(root)

    def checksums(self, root):
        (root / "SHA256SUMS").write_text("".join(
            f"{hashlib.sha256(p.read_bytes()).hexdigest()}  {p.name}\n"
            for p in sorted(root.iterdir()) if p.name != "SHA256SUMS"
        ))

    def test_prepares_only_linux_executables(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            prepare.prepare("v0.3.0", root, root / "context", REVISION)
            self.assertEqual(sorted(str(p.relative_to(root / "context")) for p in (root / "context").rglob("outpace")),
                             ["amd64/outpace", "arm-v7/outpace", "arm64/outpace"])

    def test_corrupt_archive_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            next(root.glob("*.zip")).write_bytes(b"corrupt")
            with self.assertRaises(ValueError):
                prepare.prepare("v0.3.0", root, root / "context", REVISION)

    def test_missing_target_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            next(root.glob("*.zip")).unlink()
            self.checksums(root)
            with self.assertRaises(ValueError):
                prepare.prepare("v0.3.0", root, root / "context", REVISION)

    def test_symlink_binary_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root, tarfile.SYMTYPE)
            with self.assertRaises(ValueError):
                prepare.prepare("v0.3.0", root, root / "context", REVISION)

    def test_duplicate_checksum_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            sums = root / "SHA256SUMS"
            sums.write_text(sums.read_text() + sums.read_text().splitlines()[0] + "\n")
            with self.assertRaises(ValueError):
                prepare.prepare("v0.3.0", root, root / "context", REVISION)

    def test_missing_executable_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            target = next(iter(prepare.LINUX))
            with tarfile.open(root / f"outpace-0.3.0-{target}.tar.gz", "w:gz"):
                pass
            self.checksums(root)
            with self.assertRaises(ValueError):
                prepare.prepare("v0.3.0", root, root / "context", REVISION)

    def test_missing_source_rejected_even_with_correct_outer_checksum(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            path = next(root.glob("*.zip"))
            with zipfile.ZipFile(path, "w") as archive:
                archive.writestr("missing-source.txt", "not enough")
            self.checksums(root)
            with self.assertRaises(ValueError):
                prepare.prepare("v0.3.0", root, root / "context", REVISION)

    def test_wrong_source_revision_rejected_with_correct_outer_checksum(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.fixture(root)
            with self.assertRaisesRegex(ValueError, "source identity"):
                prepare.prepare("v0.3.0", root, root / "context", "0" * 40)

    def test_invalid_tag_rejected(self):
        with self.assertRaises(ValueError):
            prepare.prepare("v0.3.0;echo unsafe", Path("unused"), Path("unused"), REVISION)


if __name__ == "__main__":
    unittest.main()
