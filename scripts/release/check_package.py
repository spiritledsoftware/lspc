#!/usr/bin/env python3
"""Exercise locked Cargo packaging and source installation."""

from __future__ import annotations

import json
import os
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
    package = next(item for item in metadata["packages"] if item["name"] == "lspctl")
    subprocess.run(["cargo", "package", "--locked", "--allow-dirty", "--no-verify"], cwd=ROOT, check=True)
    crate = ROOT / "target/package" / f"lspctl-{package['version']}.crate"
    if not crate.is_file():
        raise SystemExit("cargo package did not create the expected crate")
    with tempfile.TemporaryDirectory(
        prefix="lspctl-install-", ignore_cleanup_errors=True
    ) as temporary:
        root = Path(temporary)
        subprocess.run(
            ["cargo", "install", "--locked", "--path", ".", "--root", str(root)],
            cwd=ROOT,
            check=True,
        )
        suffix = ".exe" if os.name == "nt" else ""
        if not (root / "bin" / f"lspctl{suffix}").is_file():
            raise SystemExit("locked source installation did not install lspctl")
        if (root / "bin" / f"lspctl-fake-server{suffix}").exists():
            raise SystemExit("locked source installation included test-only fake server")
        subprocess.run(
            [str(root / "bin" / f"lspctl{suffix}"), "schema", "--full"],
            check=True,
            stdout=subprocess.DEVNULL,
        )


if __name__ == "__main__":
    main()
