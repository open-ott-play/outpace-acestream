#!/usr/bin/env python3
"""Copy actual indexed source into a new, credential-free Docker build context."""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import subprocess


PRIVATE = {'.git', 'target', 're', 'references', 'acestream-ids.txt', '.env'}
MANIFEST = 'SOURCE-MANIFEST.json'


def git(root, *args):
    return subprocess.check_output(['git', '-C', str(root), *args])


def validate(name):
    path = PurePosixPath(name)
    if (not name or path.is_absolute() or any(c in name for c in '\\\n\r')
            or '..' in path.parts or PRIVATE.intersection(path.parts) or name == MANIFEST):
        raise ValueError(f'Unsafe source path: {name}')


def prepare(root: Path, output: Path):
    root = root.resolve()
    entries = []
    for record in git(root, 'ls-files', '--stage', '-z').decode().split('\0'):
        if not record:
            continue
        attributes, name = record.split('\t', 1)
        mode, _, stage = attributes.split()
        if stage != '0':
            raise ValueError('Resolve index conflicts before preparing source')
        validate(name)
        mode = int(mode, 8)
        path = root / name
        for parent in path.parents:
            if parent == root:
                break
            if parent.is_symlink():
                raise ValueError('Source parent is a symlink')
        if mode == 0o120000:
            data = str(path.readlink()).encode() if path.is_symlink() else path.read_bytes()
            validate(data.decode())
        elif mode in (0o100644, 0o100755) and path.is_file() and not path.is_symlink():
            data = path.read_bytes()
            if os.name != "nt":
                mode = 0o100755 if path.stat().st_mode & 0o111 else 0o100644
        else:
            raise ValueError(f'Unsupported source file: {name}')
        entries.append((name, mode, data))
    # Check all inputs before writing any output. No ignored/untracked content is read.
    output.mkdir(parents=True, exist_ok=False)
    files = []
    for name, mode, data in sorted(entries):
        destination = output / name
        destination.parent.mkdir(parents=True, exist_ok=True)
        if mode == 0o120000:
            destination.symlink_to(data.decode())
        else:
            destination.write_bytes(data)
            destination.chmod(mode & 0o777)
        files.append({'path': name, 'mode': mode, 'sha256': hashlib.sha256(data).hexdigest()})
    manifest = {
        'format': 1,
        'base_revision': git(root, 'rev-parse', 'HEAD').decode().strip(),
        'tree_state': 'modified' if git(root, 'status', '--porcelain', '--untracked-files=no') else 'clean',
        'files': files,
    }
    (output / MANIFEST).write_text(json.dumps(manifest, indent=2) + '\n')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('output', type=Path)
    args = parser.parse_args()
    prepare(Path(__file__).resolve().parents[2], args.output)
