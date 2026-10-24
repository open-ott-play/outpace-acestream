#!/usr/bin/env python3
"""Package a build's workspace source, checksum-verified locked dependencies and notices."""
import argparse
import gzip
import hashlib
import io
import json
from pathlib import Path, PurePosixPath
import subprocess
import tarfile
import tempfile
import tomllib

PREFIX = 'outpace-acestream-complete'
ENGINE_PREFIX = 'outpace-acestream'
SUPPLEMENTS = Path(__file__).with_name('licenses')
# These published crate packages omit their license text. Preserve the license
# from their corresponding upstream revision beside, not inside, vendored code.
SUPPLEMENT_FILES = {
    'crab_nat-0.8.1': 'crab_nat-MIT.txt',
    'jni-0.22.4': 'jni-MIT.txt',
    'jni-macros-0.22.4': 'jni-MIT.txt',
    'jni-sys-macros-0.4.1': 'jni-sys-MIT.txt',
    'rustls-platform-verifier-android-0.2.0': 'rustls-platform-verifier-MIT.txt',
}


def safe_relative(value):
    path = PurePosixPath(value)
    if not value or path.is_absolute() or '..' in path.parts or '\\' in value:
        raise ValueError(f'Unsafe dependency path: {value}')
    return path


def source_lock(engine):
    with tarfile.open(engine, 'r:gz') as archive:
        matches = [m for m in archive.getmembers() if m.name == ENGINE_PREFIX + '/Cargo.lock']
        if len(matches) != 1 or not matches[0].isfile():
            raise ValueError('Build source must contain one Cargo.lock')
        return archive.extractfile(matches[0]).read()


def dependencies(vendor, lock):
    expected = {}
    for package in tomllib.loads(lock.decode())['package']:
        if 'source' not in package:
            continue
        if package['source'] != 'registry+https://github.com/rust-lang/crates.io-index':
            raise ValueError('Add explicit source handling before publishing non-crates.io dependencies')
        expected[package['name'] + '-' + package['version']] = package
    actual = {p.name for p in vendor.iterdir()}
    if actual != set(expected):
        raise ValueError('Vendor inventory does not match the exact Cargo.lock')
    notices = ['Third-party software distributed with Outpace AceStream',
               'Original license declarations and notices follow. Complete dependency sources are included in the corresponding-source asset.', '']
    inventory = []
    for name, package in sorted(expected.items()):
        root = vendor / name
        if root.is_symlink() or not root.is_dir():
            raise ValueError('Expected regular dependency directory')
        checksum_path = root / '.cargo-checksum.json'
        if checksum_path.is_symlink():
            raise ValueError('Dependency checksum metadata must be a regular file')
        checksum = json.loads(checksum_path.read_text())
        if set(checksum) - {'package', 'files', '$comment'}:
            raise ValueError('Unexpected dependency checksum metadata')
        if '$comment' in checksum and checksum['$comment'] != 'This file only protects against accidental modifications. It is not a security mechanism and does not protect against malicious changes.':
            raise ValueError('Unexpected dependency checksum comment')
        if checksum['package'] != package['checksum']:
            raise ValueError('Dependency registry checksum differs from Cargo.lock')
        files = checksum['files']
        actual_files = set()
        for path in root.rglob('*'):
            if path.is_symlink():
                raise ValueError('Symlinks are not supported in vendored dependency packages')
            if path.is_file():
                relative = path.relative_to(root).as_posix()
                if relative != '.cargo-checksum.json':
                    actual_files.add(relative)
        if actual_files != set(files):
            raise ValueError('Unexpected or missing dependency source files')
        for relative, digest in files.items():
            path = root / safe_relative(relative)
            if hashlib.sha256(path.read_bytes()).hexdigest() != digest:
                raise ValueError(f'Changed dependency source: {name}/{relative}')
        info = tomllib.loads((root / 'Cargo.toml').read_text())['package']
        legal = []
        explicit = info.get('license-file')
        for relative in sorted(files):
            base = PurePosixPath(relative).name.lower()
            if relative == explicit or base.startswith(('license', 'licence', 'copying', 'copyright', 'notice', 'authors')):
                if not base.endswith(('.rs', '.png', '.jpg')):
                    legal.append((relative, (root / relative).read_bytes()))
        if not legal:
            supplement = SUPPLEMENT_FILES.get(name)
            if not supplement or not (SUPPLEMENTS / supplement).is_file():
                raise ValueError(f'Missing license text: {name}; add a reviewed upstream supplement')
            legal.append(('upstream license supplement/' + supplement, (SUPPLEMENTS / supplement).read_bytes()))
        declared = info.get('license', 'See license file')
        authors = ', '.join(info.get('authors', []))
        notices.extend(['=' * 72, name, 'Declared license: ' + declared, 'Authors: ' + authors])
        for relative, data in legal:
            notices.extend(['--- ' + relative + ' ---', data.decode('utf-8', errors='replace'), ''])
        inventory.append({'name': package['name'], 'version': package['version'], 'license': declared,
                          'registry_checksum': package['checksum'], 'license_files': [path for path, _ in legal]})
    return ('\n'.join(notices) + '\n').encode(), inventory


def add_bytes(archive, name, data, mode=0o644):
    header = tarfile.TarInfo(PREFIX + '/' + name)
    header.mode = mode
    header.size = len(data)
    archive.addfile(header, io.BytesIO(data))


def prepare(package, vendor, output):
    engine = package / 'corresponding-source.tar.gz'
    info = json.loads((package / 'source.json').read_text())
    if hashlib.sha256(engine.read_bytes()).hexdigest() != info['source_sha256']:
        raise ValueError('Embedded workspace source checksum mismatch')
    lock = source_lock(engine)
    notices, inventory = dependencies(vendor, lock)
    # Byte writes preserve LF on Windows; all targets receive the same notices.
    (package / 'THIRD_PARTY_NOTICES.txt').write_bytes(notices)
    instructions = '''# Rebuild the exact source tree

This bundle contains engine-source.tar.gz (identical to the executable's embedded
workspace archive), every Cargo.lock registry dependency under vendor/, their
original notices, and source.json identifying the build. No Rust compiler,
linker or operating-system libraries are included; install the toolchain in the
engine's rust-toolchain.toml and the system requirements in its Dockerfile.

Extract engine-source.tar.gz in this directory, then enter outpace-acestream.
Run this command (PowerShell and POSIX shells both support these quoted options):

    cargo --config 'source.crates-io.replace-with="vendored-sources"' --config 'source.vendored-sources.directory="../vendor"' build --offline --frozen --release -p ace-engine --bin outpace

For a cross build, append --target with the target recorded in source.json and
install the corresponding compiler/linker first. The source archive is the
identity of the build inputs, not a guarantee of bit-identical output under a
different native toolchain. A rebuilt or modified local binary does not claim
to be the published release and embeds its own current workspace sources.
'''
    output.parent.mkdir(parents=True, exist_ok=True)
    # No source timestamps, absolute paths, host user IDs or local cache files.
    with output.open('wb') as target, gzip.GzipFile(fileobj=target, mode='wb', mtime=0, filename='') as compressed:
        with tarfile.open(fileobj=compressed, mode='w|', format=tarfile.PAX_FORMAT) as archive:
            add_bytes(archive, 'engine-source.tar.gz', engine.read_bytes())
            add_bytes(archive, 'source.json', (package / 'source.json').read_bytes())
            add_bytes(archive, 'THIRD_PARTY_NOTICES.txt', notices)
            add_bytes(archive, 'DEPENDENCIES.json', (json.dumps(inventory, indent=2) + '\n').encode())
            add_bytes(archive, 'REBUILD.md', instructions.encode())
            for path in sorted(vendor.rglob('*')):
                if path.is_file():
                    add_bytes(archive, 'vendor/' + path.relative_to(vendor).as_posix(), path.read_bytes(), 0o755 if path.stat().st_mode & 0o111 else 0o644)
    return len(inventory)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('package', type=Path, help='directory produced by package_source.py')
    parser.add_argument('output', type=Path)
    parser.add_argument('--vendor', type=Path, help='already generated cargo vendor directory (checksums are still verified)')
    args = parser.parse_args()
    if args.vendor:
        count = prepare(args.package, args.vendor, args.output)
    else:
        root = Path(__file__).resolve().parents[2]
        if source_lock(args.package / 'corresponding-source.tar.gz') != (root / 'Cargo.lock').read_bytes():
            raise SystemExit('Working Cargo.lock differs from the build source')
        with tempfile.TemporaryDirectory(prefix='outpace-vendor-') as tmp:
            vendor = Path(tmp) / 'vendor'
            subprocess.run(['cargo', 'vendor', '--locked', '--versioned-dirs', str(vendor)], cwd=root, check=True, stdout=subprocess.DEVNULL)
            count = prepare(args.package, vendor, args.output)
    print(f'Packaged {count} locked dependency sources and their notices into {args.output.name}')
