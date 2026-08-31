#!/usr/bin/env python3
"""Verify the runtime schema output is the checked v1 contract."""

from __future__ import annotations

import json
import subprocess
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]


def load(path: Path) -> object:
    return json.loads(path.read_text(encoding="utf-8"))


def main() -> None:
    output = subprocess.run(
        ["cargo", "run", "--locked", "--quiet", "--", "schema", "--full"],
        cwd=ROOT,
        check=True,
        capture_output=True,
    )
    envelope = json.loads(output.stdout)
    result = envelope["result"]
    if envelope["schemaVersion"] != 1 or not envelope["ok"]:
        raise SystemExit("schema command did not return a v1 success envelope")
    if result["catalog"] != load(ROOT / "assets/contract/catalog.json"):
        raise SystemExit("runtime catalog differs from the checked catalog")
    if result["schemas"] != load(ROOT / "assets/contract/schemas.json"):
        raise SystemExit("runtime schema registry differs from the checked registry")


if __name__ == "__main__":
    main()
