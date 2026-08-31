#!/usr/bin/env python3
"""Exercise locked Cargo packaging and source installation."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def main() -> None:
    metadata = json.loads(
        subprocess.run(
            ["cargo", "metadata", "--locked", "--no-deps", "--format-version=1"],
            cwd=ROOT,
            check=True,
            text=True,
            capture_output=True,
        ).stdout
    )
    package = next(item for item in metadata["packages"] if item["name"] == "lspc")
    subprocess.run(["cargo", "package", "--locked", "--allow-dirty", "--no-verify"], cwd=ROOT, check=True)
    crate = ROOT / "target/package" / f"lspc-{package['version']}.crate"
    if not crate.is_file():
        raise SystemExit("cargo package did not create the expected crate")
    root = Path(tempfile.mkdtemp(prefix="lspc-install-"))
    try:
        subprocess.run(
            ["cargo", "install", "--locked", "--path", ".", "--root", str(root)],
            cwd=ROOT,
            check=True,
        )
        suffix = ".exe" if os.name == "nt" else ""
        if not (root / "bin" / f"lspc{suffix}").is_file():
            raise SystemExit("locked source installation did not install lspc")
    finally:
        shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    main()
