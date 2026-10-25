#!/usr/bin/env python3
"""Verify the complete release archive set and copy only Linux executables."""
import argparse
import hashlib
import json
import zipfile
from pathlib import Path
import re
import stat
import tarfile

from release_policy import parse_tag

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


def read_executable(archive, prefix: str, *, windows: bool, retain: bool):
    """Validate every target's executable, retaining only Linux container bytes."""
    name = prefix + ("outpace.exe" if windows else "outpace")
    if windows:
        matches = [item for item in archive.infolist() if item.filename.replace("\\", "/") == name]
        if (len(matches) != 1 or matches[0].is_dir()
                or matches[0].external_attr & 0x10
                or stat.S_IFMT(matches[0].external_attr >> 16) not in (0, stat.S_IFREG)):
            raise ValueError(f"Expected exactly one regular executable: {name}")
        member = matches[0]
        size = member.file_size
        source = archive.open(member)
    else:
        matches = [item for item in archive.getmembers() if item.name == name]
        if len(matches) != 1 or not matches[0].isfile() or not matches[0].mode & 0o111:
            raise ValueError(f"Expected exactly one regular executable: {name}")
        member = matches[0]
        size = member.size
        source = archive.extractfile(member)
    with source:
        if size == 0:
            raise ValueError(f"Empty executable: {name}")
        if retain:
            data = source.read()
            actual_size = len(data)
        else:
            # Reading also verifies ZIP CRC/truncation without retaining binaries
            # that will not be copied into the Linux image context.
            actual_size = 0
            while chunk := source.read(1024 * 1024):
                actual_size += len(chunk)
            data = None
        if actual_size != size:
            raise ValueError(f"Truncated executable: {name}")
        return data


def prepare(tag: str, assets: Path, output: Path, revision: str) -> None:
    if not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("Expected the resolved Git release commit")
    parse_tag(tag)
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
    binaries = {}
    for target, extension in TARGETS.items():
        path = assets / f"outpace-{version}-{target}.{extension}"
        prefix = f"outpace-{version}-{target}/"
        required = ["corresponding-source.tar.gz", "source.json", "LICENSE", "NOTICE", "AUTHORS.md", "THIRD_PARTY_NOTICES.txt"]
        values = {}
        if extension == "zip":
            with zipfile.ZipFile(path) as archive:
                read_executable(archive, prefix, windows=True, retain=False)
                for name in required:
                    # PowerShell Compress-Archive uses backslashes on Windows.
                    # Read exact normalized members without extracting paths;
                    # slash/backslash aliases must still count as duplicates.
                    matches = [item for item in archive.infolist() if item.filename.replace("\\", "/") == prefix + name]
                    if (len(matches) != 1 or matches[0].is_dir()
                            or matches[0].external_attr & 0x10
                            or stat.S_IFMT(matches[0].external_attr >> 16) not in (0, stat.S_IFREG)
                            or matches[0].file_size > 64 * 1024 * 1024):
                        raise ValueError(f"Missing or oversized release source member: {name}")
                    values[name] = archive.read(matches[0])
        else:
            with tarfile.open(path, "r:gz") as archive:
                binary = read_executable(archive, prefix, windows=False, retain=target in LINUX)
                if target in LINUX:
                    binaries[LINUX[target]] = binary
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
    # Validate every target before creating the context. Never extract paths.
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
