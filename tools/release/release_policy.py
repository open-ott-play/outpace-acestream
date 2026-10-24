#!/usr/bin/env python3
"""Shared tag, GitHub release and container-channel publication policy."""
import argparse
import json
from pathlib import Path
import re
from typing import NamedTuple


class Version(NamedTuple):
    major: int
    minor: int
    patch: int
    beta: int | None

    @property
    def channel(self) -> str:
        return "latest" if self.beta is None else "beta"


def parse_tag(tag: str) -> Version:
    number = r"(0|[1-9][0-9]*)"
    match = re.fullmatch(rf"v{number}\.{number}\.{number}(?:-beta\.([1-9][0-9]*))?", tag)
    if match is None:
        raise ValueError("Expected vX.Y.Z or vX.Y.Z-beta.N (N starts at 1; no leading zeroes)")
    return Version(*(int(part) if part is not None else None for part in match.groups()))


def metadata(tag: str, cargo_version: str | None = None) -> dict[str, str]:
    version = parse_tag(tag)
    if cargo_version is not None and tag != f"v{cargo_version}":
        raise ValueError("Release tag does not match the ace-engine Cargo version")
    beta = version.beta is not None
    return {
        "version": tag[1:],
        "prerelease": str(beta).lower(),
        "make_latest": str(not beta).lower(),
        "channel": version.channel,
    }


def validate_release(tag: str, release: object, *, draft: bool = False) -> None:
    """Check the JSON returned by gh release view before using its assets."""
    version = parse_tag(tag)
    if (not isinstance(release, dict) or release.get("tagName") != tag
            or release.get("isDraft") is not draft
            or release.get("isPrerelease") is not (version.beta is not None)):
        state = "draft" if draft else "published"
        raise ValueError(f"Expected the exact {state} release with matching prerelease status")


def release_list(document: object) -> list[dict]:
    """Validate the paginated REST API shape; missing metadata fails closed."""
    if not isinstance(document, list):
        raise ValueError("Expected a complete GitHub release list")
    for release in document:
        if (not isinstance(release, dict) or not isinstance(release.get("tag_name"), str)
                or type(release.get("draft")) is not bool
                or type(release.get("prerelease")) is not bool):
            raise ValueError("Incomplete GitHub release metadata")
    tags = [release["tag_name"] for release in document]
    if len(tags) != len(set(tags)):
        raise ValueError("Duplicate GitHub release tags")
    return document


def ensure_unpublished(tag: str, document: object) -> None:
    """A beta upload can create/resume a draft, never unpublish an existing release."""
    version = parse_tag(tag)
    if version.beta is None:
        raise ValueError("Draft publication is reserved for beta releases")
    for release in release_list(document):
        if release["tag_name"] == tag:
            if not release["draft"] or not release["prerelease"]:
                raise ValueError("Cannot replace an existing published or non-beta release")


def check_channel_advance(tag: str, document: object) -> None:
    """Prevent older versions from replacing either moving container channel."""
    requested = parse_tag(tag)
    found = False
    for release in release_list(document):
        if release["draft"]:
            continue
        try:
            version = parse_tag(release["tag_name"])
        except ValueError:
            # Historical/unrelated tag formats are not members of our channels.
            continue
        if release["prerelease"] is not (version.beta is not None):
            raise ValueError("Published release tag and prerelease status disagree")
        if release["tag_name"] == tag:
            found = True
        if version.channel == requested.channel and version > requested:
            raise ValueError(f"A newer {requested.channel} release exists; refusing to replace the channel")
    if not found:
        raise ValueError("Requested release must still be published before updating its channel")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    meta = commands.add_parser("metadata")
    meta.add_argument("tag")
    meta.add_argument("--cargo-version")
    for name in ("published", "draft", "unpublished", "channel"):
        command = commands.add_parser(name)
        command.add_argument("tag")
        command.add_argument("document", type=Path)
    args = parser.parse_args()
    try:
        if args.command == "metadata":
            for key, value in metadata(args.tag, args.cargo_version).items():
                print(f"{key}={value}")
        else:
            document = json.loads(args.document.read_text())
            if args.command in ("published", "draft"):
                validate_release(args.tag, document, draft=args.command == "draft")
            elif args.command == "unpublished":
                ensure_unpublished(args.tag, document)
            else:
                check_channel_advance(args.tag, document)
    except (ValueError, OSError) as error:
        parser.exit(1, f"Release validation failed: {error}\n")


if __name__ == "__main__":
    main()
