#!/usr/bin/env python3
"""Validate release-gate status and fail a candidate with pending work."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[2]
STATES = {"implemented", "pending"}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--require-complete", action="store_true")
    arguments = parser.parse_args()
    gates = json.loads((ROOT / "release-gates.json").read_text(encoding="utf-8"))
    if gates.get("formatVersion") != 1 or not isinstance(gates.get("gates"), list):
        raise SystemExit("release-gates.json must be format version 1 with a gates array")
    pending = []
    names = set()
    for gate in gates["gates"]:
        name, state, reason = gate.get("name"), gate.get("state"), gate.get("reason")
        if not isinstance(name, str) or not name or name in names:
            raise SystemExit("release gates need unique non-empty names")
        if state not in STATES or not isinstance(reason, str) or not reason:
            raise SystemExit(f"release gate {name!r} has an invalid state or reason")
        names.add(name)
        if state == "pending":
            pending.append(f"{name}: {reason}")
    if arguments.require_complete and pending:
        raise SystemExit("release candidate blocked by pending gates:\n" + "\n".join(pending))


if __name__ == "__main__":
    main()
