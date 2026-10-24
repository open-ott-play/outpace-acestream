#!/usr/bin/env python3
"""Copy the exact build-script source output named by Cargo's JSON build events."""
import argparse
import hashlib
import json
from pathlib import Path
import shutil


def package(events: Path, destination: Path):
    candidates = set()
    for line in events.read_text().splitlines():
        event = json.loads(line)
        if event.get('reason') == 'build-script-executed':
            out = Path(event['out_dir'])
            if (out / 'source.json').is_file():
                info = json.loads((out / 'source.json').read_text())
                if info.get('project') == 'Outpace AceStream':
                    candidates.add(out)
    if len(candidates) != 1:
        raise ValueError('Expected exactly one engine source output from this Cargo build')
    output = candidates.pop()
    source = output / 'corresponding-source.tar.gz'
    info = json.loads((output / 'source.json').read_text())
    if hashlib.sha256(source.read_bytes()).hexdigest() != info['source_sha256']:
        raise ValueError('Build source digest does not match source.json')
    shutil.copyfile(source, destination / 'corresponding-source.tar.gz')
    shutil.copyfile(output / 'source.json', destination / 'source.json')


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('events', type=Path)
    parser.add_argument('destination', type=Path)
    args = parser.parse_args()
    package(args.events, args.destination)
