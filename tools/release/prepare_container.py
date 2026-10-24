#!/usr/bin/env python3
"""Verify the complete release archive set and copy only Linux executables."""
import argparse
import hashlib
import json
import zipfile
from pathlib import Path
import re
import tarfile

TARGETS = {
    "x86_64-unknown-linux-musl": "tar.gz",
    "aarch64-unknown-linux-musl": "tar.gz",
    "armv7-unknown-linux-musleabihf": "tar.gz",
    "x86_64-apple-darwin": "tar.gz",
    "aarch64-apple-darwin": "tar.gz",
    "x86_64-pc-windows-msvc": "zip",
    "aarch64-pc-windows-msvc": "zip",
}
LINUX = {
    "x86_64-unknown-linux-musl": "amd64",
    "aarch64-unknown-linux-musl": "arm64",
    "armv7-unknown-linux-musleabihf": "arm-v7",
}


def prepare(tag: str, assets: Path, output: Path, revision: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("Expected the resolved Git release commit")
    if not re.fullmatch(r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)", tag):
        raise ValueError("Expected a stable release tag vX.Y.Z")
    version = tag[1:]
    expected = {f"outpace-{version}-{target}.{ext}" for target, ext in TARGETS.items()}
    expected |= {f"outpace-source-{version}-{target}-{revision}.tar.gz" for target in TARGETS}
    if {p.name for p in assets.iterdir()} != expected | {"SHA256SUMS"}:
        raise ValueError("Release must contain exactly all seven binary and seven source archives and SHA256SUMS")
    checksums = {}
    for line in (assets / "SHA256SUMS").read_text().splitlines():
        match = re.fullmatch(r"([0-9a-f]{64})  (outpace-[^/\\\s]+)", line)
        if not match or match[2] in checksums:
            raise ValueError("Invalid or duplicate checksum entry")
        checksums[match[2]] = match[1]
    if set(checksums) != expected:
        raise ValueError("Checksums must cover exactly all seven binary and seven source archives")
    for name, digest in checksums.items():
        archive = assets / name
        if archive.is_symlink() or not archive.is_file():
            raise ValueError(f"Not a regular archive: {name}")
        with archive.open("rb") as source:
            actual = hashlib.file_digest(source, "sha256").hexdigest()
        if actual != digest:
            raise ValueError(f"Checksum mismatch: {name}")
    # Every platform must ship its exact source and legal notices. Read selected
    # regular members only; never extract user-controlled archive paths.
    notices = {}
    for target, extension in TARGETS.items():
        path = assets / f"outpace-{version}-{target}.{extension}"
        prefix = f"outpace-{version}-{target}/"
        required = ["corresponding-source.tar.gz", "source.json", "LICENSE", "NOTICE", "AUTHORS.md", "THIRD_PARTY_NOTICES.txt"]
        values = {}
        if extension == "zip":
            with zipfile.ZipFile(path) as archive:
                for name in required:
                    matches = [item for item in archive.infolist() if item.filename == prefix + name]
                    if len(matches) != 1 or matches[0].is_dir() or matches[0].file_size > 64 * 1024 * 1024:
                        raise ValueError(f"Missing or oversized release source member: {name}")
                    values[name] = archive.read(matches[0])
        else:
            with tarfile.open(path, "r:gz") as archive:
                for name in required:
                    matches = [item for item in archive.getmembers() if item.name == prefix + name]
                    if len(matches) != 1 or not matches[0].isfile() or matches[0].size > 64 * 1024 * 1024:
                        raise ValueError(f"Missing or oversized release source member: {name}")
                    values[name] = archive.extractfile(matches[0]).read()
        info = json.loads(values["source.json"])
        if (info.get("version") != version or info.get("license") != "AGPL-3.0-or-later"
                or info.get("project") != "Outpace AceStream"
                or info.get("repository") != "https://github.com/open-ott-play/outpace-acestream"
                or info.get("target") != target or info.get("base_revision") != revision
                or info.get("tree_state") != "clean"
                or hashlib.sha256(values["corresponding-source.tar.gz"]).hexdigest() != info.get("source_sha256")):
            raise ValueError("Release source identity or checksum mismatch")
        complete_name = f"outpace-source-{version}-{target}-{revision}.tar.gz"
        complete_url = f"https://github.com/open-ott-play/outpace-acestream/releases/download/{tag}/{complete_name}"
        if info.get("complete_source_url") != complete_url:
            raise ValueError("Release binary does not offer its exact complete source asset")
        with tarfile.open(assets / complete_name, "r:gz") as complete:
            for filename, expected_bytes in [("source.json", values["source.json"]), ("engine-source.tar.gz", values["corresponding-source.tar.gz"]), ("THIRD_PARTY_NOTICES.txt", values["THIRD_PARTY_NOTICES.txt"])]:
                matches = [entry for entry in complete.getmembers() if entry.name == "outpace-acestream-complete/" + filename]
                if len(matches) != 1 or not matches[0].isfile() or complete.extractfile(matches[0]).read() != expected_bytes:
                    raise ValueError("Complete source asset does not match binary package")
            if not any(entry.name.startswith("outpace-acestream-complete/vendor/") for entry in complete.getmembers()):
                raise ValueError("Complete source asset lacks registry dependencies")
        for name in ["LICENSE", "NOTICE", "AUTHORS.md", "THIRD_PARTY_NOTICES.txt"]:
            if name in notices and notices[name] != values[name]:
                raise ValueError("Legal notices differ across release targets")
            notices[name] = values[name]
    # Validate every selected member before creating the context. Never extract paths.
    binaries = {}
    for target, platform in LINUX.items():
        with tarfile.open(assets / f"outpace-{version}-{target}.tar.gz", "r:gz") as archive:
            name = f"outpace-{version}-{target}/outpace"
            matches = [member for member in archive.getmembers() if member.name == name]
            if len(matches) != 1 or not matches[0].isfile() or not matches[0].mode & 0o111:
                raise ValueError(f"Expected exactly one regular executable: {name}")
            with archive.extractfile(matches[0]) as source:
                binaries[platform] = source.read()
            if not binaries[platform]:
                raise ValueError(f"Empty executable: {name}")
    output.mkdir(parents=True, exist_ok=False)
    for name, data in notices.items():
        (output / name).write_bytes(data)
    for platform, binary in binaries.items():
        destination = output / platform / "outpace"
        destination.parent.mkdir()
        destination.write_bytes(binary)
        destination.chmod(0o755)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("tag")
    parser.add_argument("assets", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("revision", help="resolved commit of the release tag")
    args = parser.parse_args()
    prepare(args.tag, args.assets, args.output, args.revision)
