#!/usr/bin/env python3
"""Build one native release archive from the locked source tree."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def command(arguments: list[str]) -> str:
    return subprocess.run(arguments, cwd=ROOT, check=True, text=True, capture_output=True).stdout.strip()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--skill-only", action="store_true")
    parser.add_argument("--commit", default=os.environ.get("GITHUB_SHA"))
    arguments = parser.parse_args()
    if not arguments.commit:
        arguments.commit = command(["git", "rev-parse", "HEAD"])
    metadata = json.loads(command(["cargo", "metadata", "--locked", "--no-deps", "--format-version=1"]))
    package = next(item for item in metadata["packages"] if item["name"] == "lspc")
    target = None if arguments.skill_only else arguments.target
    suffix = ".exe" if ("windows" in arguments.target if target else os.name == "nt") else ""
    build = ["cargo", "build", "--locked", "--release"]
    if target:
        build.extend(("--target", target))
    subprocess.run(build, cwd=ROOT, check=True)
    binary = ROOT / "target" / (Path(arguments.target) / "release" if target else Path("release")) / f"lspc{suffix}"
    rust_version = command(["rustc", "-Vv"]).splitlines()[0]
    if target:
        subprocess.run([sys.executable, str(ROOT / "scripts/release/build_archive.py"), "--binary", str(binary), "--target", arguments.target, "--version", package["version"], "--commit", arguments.commit, "--rust-version", rust_version, "--output-dir", str(arguments.output_dir)], cwd=ROOT, check=True)
    schema = subprocess.run([str(binary), "schema", "--full"], check=True, capture_output=True).stdout
    with tempfile.TemporaryDirectory(prefix="lspc-skill-schema-") as temporary:
        schema_path = Path(temporary) / "schema.json"
        schema_path.write_bytes(schema)
        subprocess.run([sys.executable, str(ROOT / "scripts/release/build_skill_archive.py"), "--version", package["version"], "--schema-json", str(schema_path), "--output-dir", str(arguments.output_dir)], cwd=ROOT, check=True)


if __name__ == "__main__":
    main()
