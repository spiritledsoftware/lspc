#!/usr/bin/env python3
"""Verify a release archive without trusting the archive builder."""

from __future__ import annotations

import argparse
import hashlib
import json
import tarfile
import zipfile
from pathlib import Path, PurePosixPath


def sha256(content: bytes) -> str:
    return f"sha256:{hashlib.sha256(content).hexdigest()}"


def entries(path: Path) -> dict[str, bytes]:
    if path.suffix == ".zip":
        with zipfile.ZipFile(path) as archive:
            return {name: archive.read(name) for name in archive.namelist() if not name.endswith("/")}
    with tarfile.open(path) as archive:
        return {
            member.name: archive.extractfile(member).read()
            for member in archive.getmembers()
            if member.isfile()
        }


def fail(message: str) -> None:
    raise SystemExit(f"invalid release archive: {message}")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("archive", type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    arguments = parser.parse_args()
    archive = arguments.archive
    if not archive.is_file() or archive.is_symlink():
        fail("archive is missing or unsafe")
    prefix = f"lspc-v{arguments.version}-{arguments.target}"
    contents = entries(archive)
    manifest_path = f"{prefix}/manifest.json"
    if manifest_path not in contents:
        fail("manifest is absent")
    try:
        manifest = json.loads(contents[manifest_path])
    except (UnicodeDecodeError, json.JSONDecodeError):
        fail("manifest is not JSON")
    if not isinstance(manifest, dict) or manifest.get("formatVersion") != 1:
        fail("manifest format version is unsupported")
    if manifest.get("name") != "lspc" or manifest.get("version") != arguments.version:
        fail("manifest identity does not match the requested release")
    if manifest.get("target") != arguments.target:
        fail("manifest target does not match the requested target")
    files = manifest.get("files")
    if not isinstance(files, list) or not files:
        fail("manifest files are absent")
    declared = set()
    for file in files:
        if not isinstance(file, dict):
            fail("manifest contains a non-object file")
        path, digest, size = file.get("path"), file.get("sha256"), file.get("bytes")
        if not isinstance(path, str) or not path or path in declared:
            fail("manifest has an invalid or duplicate file path")
        relative = PurePosixPath(path)
        if relative.is_absolute() or ".." in relative.parts:
            fail("manifest has an escaping file path")
        content = contents.get(f"{prefix}/{path}")
        if content is None:
            fail(f"manifest file is absent: {path}")
        if not isinstance(size, int) or size < 0 or len(content) != size:
            fail(f"manifest size is wrong: {path}")
        if digest != sha256(content):
            fail(f"manifest digest is wrong: {path}")
        declared.add(path)
    actual = {name.removeprefix(f"{prefix}/") for name in contents if name != manifest_path}
    if actual != declared:
        fail("archive payload does not exactly match its manifest")
    required = {"README.md", "LICENSE-MIT", "LICENSE-APACHE", "lspc.exe" if "windows" in arguments.target else "lspc"}
    if not required <= declared or not any(path.startswith("skills/lspc/") for path in declared):
        fail("archive lacks a required release payload")
    checksum = archive.with_suffix(archive.suffix + ".sha256")
    expected_checksum = f"{hashlib.sha256(archive.read_bytes()).hexdigest()}  {archive.name}\n"
    if not checksum.is_file() or checksum.read_text(encoding="ascii") != expected_checksum:
        fail("archive checksum sidecar is wrong")


if __name__ == "__main__":
    main()
