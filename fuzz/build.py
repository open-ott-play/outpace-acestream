#!/usr/bin/env python3
"""Build bounded offline targets with the separately pinned fuzz toolchain."""

import os
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[1]
NIGHTLY = "nightly-2026-10-12"
TARGETS = ("metadata", "peer_frames", "peer_extensions", "media_resync", "hls_playlist", "tracker_responses")


def main():
    environment = os.environ.copy()
    environment["PATH"] = str(ROOT / ".fuzz-tools/bin") + os.pathsep + environment["PATH"]
    environment["CARGO_BUILD_JOBS"] = "2"
    lock = ROOT / "fuzz/Cargo.lock"
    before = lock.read_bytes()
    # Check the dependency lock before invoking cargo-fuzz, which does not expose
    # Cargo's --locked flag. Also reject any change made during the build.
    subprocess.run(["cargo", "+" + NIGHTLY, "fetch", "--manifest-path", "fuzz/Cargo.toml", "--locked"],
                   cwd=ROOT, env=environment, check=True, timeout=180)
    version = subprocess.check_output(["cargo", "+" + NIGHTLY, "fuzz", "--version"],
                                      cwd=ROOT, env=environment, text=True, timeout=30).strip()
    if version != "cargo-fuzz 0.13.1":
        raise ValueError("The fuzz builder requires cargo-fuzz 0.13.1")
    target = next(line.removeprefix("host: ") for line in subprocess.check_output(
        ["rustc", "+" + NIGHTLY, "-vV"], cwd=ROOT, env=environment, text=True, timeout=30,
    ).splitlines() if line.startswith("host: "))
    subprocess.run([
        "cargo", "+" + NIGHTLY, "fuzz", "build", "--fuzz-dir", "fuzz", "--features", "fuzzing",
        "--target-dir", str(ROOT / ".fuzz-build"), "--target", target,
    ], cwd=ROOT, env=environment, check=True, timeout=540)
    if lock.read_bytes() != before:
        raise ValueError("The fuzz build changed Cargo.lock; review dependency resolution")
    destination = ROOT / ".fuzz-build/bin"
    destination.mkdir(parents=True, exist_ok=True)
    for name in TARGETS:
        shutil.copy2(ROOT / ".fuzz-build" / target / "release" / name, destination / name)


if __name__ == "__main__":
    main()
