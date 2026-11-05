#!/usr/bin/env python3
"""Reject native release binaries that fail or emit startup diagnostics."""
import argparse
import os
from pathlib import Path
import subprocess


def check(binary: Path, version: str) -> None:
    env = os.environ.copy()
    # Exercise the defaults shipped in the executable, not runner overrides.
    for name in ("MALLOC_CONF", "_RJEM_MALLOC_CONF"):
        env.pop(name, None)
    result = subprocess.run([str(binary.resolve()), "--version"], env=env,
                            capture_output=True, text=True, timeout=15)
    if result.returncode != 0 or result.stdout.strip() != "outpace " + version or result.stderr:
        raise SystemExit(f"Native startup check failed: exit={result.returncode}, "
                         f"stdout={result.stdout[:2000]!r}, stderr={result.stderr[:2000]!r}")
    print(result.stdout.strip() + ": silent startup verified")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("version")
    args = parser.parse_args()
    check(args.binary, args.version)
