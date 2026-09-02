#!/usr/bin/env python3
"""Idempotently publish the exact tag-tested crate and attested release assets."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import subprocess
import tempfile
import tomllib
import urllib.error
import urllib.request
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
TARGETS = [
    ("x86_64-unknown-linux-gnu", "tar.gz"),
    ("aarch64-unknown-linux-gnu", "tar.gz"),
    ("x86_64-apple-darwin", "tar.gz"),
    ("aarch64-apple-darwin", "tar.gz"),
    ("x86_64-pc-windows-msvc", "zip"),
]


def run(arguments: list[str]) -> None:
    subprocess.run(
        arguments,
        cwd=ROOT,
        check=True,
    )


def sha256(path: Path) -> str:
    return hashlib.sha256(path.read_bytes()).hexdigest()


def expected_assets(directory: Path, version: str) -> list[Path]:
    archives = [
        directory / f"lspc-v{version}-{target}.{extension}"
        for target, extension in TARGETS
    ]
    archives.append(directory / f"lspc-agent-skill-v{version}.zip")
    assets = []
    for archive in archives:
        checksum = archive.with_suffix(archive.suffix + ".sha256")
        if not archive.is_file() or archive.is_symlink() or not checksum.is_file():
            raise SystemExit(f"release asset is missing or unsafe: {archive}")
        expected = checksum.read_text(encoding="ascii").split()[0]
        if expected != sha256(archive):
            raise SystemExit(f"release asset checksum is invalid: {archive.name}")
        assets.extend((archive, checksum))
    return assets


def release_state(tag: str) -> dict[str, object] | None:
    completed = subprocess.run(
        ["gh", "release", "view", tag, "--json", "isDraft,assets"],
        cwd=ROOT,
        text=True,
        capture_output=True,
    )
    if completed.returncode != 0:
        return None
    return json.loads(completed.stdout)


def ensure_release(tag: str, notes: Path) -> dict[str, object]:
    state = release_state(tag)
    if state is None:
        run(
            [
                "gh",
                "release",
                "create",
                tag,
                "--verify-tag",
                "--draft",
                "--title",
                tag,
                "--notes-file",
                str(notes),
            ]
        )
        state = release_state(tag)
    assert state is not None
    return state


def upload_missing_assets(tag: str, assets: list[Path], state: dict[str, object]) -> None:
    existing = {asset["name"] for asset in state["assets"]}
    for asset in assets:
        if asset.name not in existing:
            run(["gh", "release", "upload", tag, str(asset)])
            continue
        with tempfile.TemporaryDirectory(prefix="lspc-release-asset-") as temporary:
            run(
                [
                    "gh",
                    "release",
                    "download",
                    tag,
                    "--pattern",
                    asset.name,
                    "--dir",
                    temporary,
                ]
            )
            published = Path(temporary) / asset.name
            if sha256(published) != sha256(asset):
                raise SystemExit(
                    f"published asset differs and will not be replaced: {asset.name}"
                )


def crates_io_checksum(version: str) -> str | None:
    request = urllib.request.Request(
        f"https://crates.io/api/v1/crates/lspc/{version}",
        headers={"User-Agent": "lspc-release-workflow/1"},
    )
    try:
        with urllib.request.urlopen(request, timeout=30) as response:
            return json.load(response)["version"]["checksum"]
    except urllib.error.HTTPError as error:
        if error.code == 404:
            return None
        raise


def publish_crate(version: str) -> None:
    run(["cargo", "package", "--locked"])
    package = ROOT / "target/package" / f"lspc-{version}.crate"
    local_checksum = sha256(package)
    published_checksum = crates_io_checksum(version)
    if published_checksum is None:
        if not os.environ.get("CARGO_REGISTRY_TOKEN"):
            raise SystemExit("CARGO_REGISTRY_TOKEN is required")
        run(["cargo", "publish", "--locked"])
    elif published_checksum != local_checksum:
        raise SystemExit("the published crate checksum differs from this immutable tag")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--tag", required=True)
    parser.add_argument("--assets", type=Path, required=True)
    arguments = parser.parse_args()
    metadata = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    version = metadata["package"]["version"]
    if arguments.tag != f"v{version}":
        raise SystemExit(f"tag {arguments.tag!r} does not match crate version {version!r}")
    notes = ROOT / "docs/releases" / f"v{version}.md"
    if not notes.is_file():
        raise SystemExit(f"release notes are missing: {notes}")
    if not os.environ.get("CARGO_REGISTRY_TOKEN"):
        raise SystemExit("CARGO_REGISTRY_TOKEN is required")
    assets = expected_assets(arguments.assets.resolve(), version)
    state = ensure_release(arguments.tag, notes)
    upload_missing_assets(arguments.tag, assets, state)
    publish_crate(version)
    if bool(state["isDraft"]):
        run(["gh", "release", "edit", arguments.tag, "--draft=false"])


if __name__ == "__main__":
    main()
