#!/usr/bin/env python3
"""Build one native release archive from the locked source tree."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def command(arguments: list[str]) -> str:
    return subprocess.run(arguments, cwd=ROOT, check=True, text=True, capture_output=True).stdout.strip()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--target", required=True)
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--commit", default=os.environ.get("GITHUB_SHA"))
    arguments = parser.parse_args()
    if not arguments.commit:
        arguments.commit = command(["git", "rev-parse", "HEAD"])
    metadata = json.loads(command(["cargo", "metadata", "--locked", "--no-deps", "--format-version=1"]))
    package = next(item for item in metadata["packages"] if item["name"] == "lspc")
    suffix = ".exe" if "windows" in arguments.target else ""
    subprocess.run(["cargo", "build", "--locked", "--release", "--target", arguments.target], cwd=ROOT, check=True)
    binary = ROOT / "target" / arguments.target / "release" / f"lspc{suffix}"
    rust_version = command(["rustc", "-Vv"]).splitlines()[0]
    subprocess.run(
        [
            sys.executable,
            str(ROOT / "scripts/release/build_archive.py"),
            "--binary",
            str(binary),
            "--target",
            arguments.target,
            "--version",
            package["version"],
            "--commit",
            arguments.commit,
            "--rust-version",
            rust_version,
            "--output-dir",
            str(arguments.output_dir),
        ],
        cwd=ROOT,
        check=True,
    )


if __name__ == "__main__":
    main()
