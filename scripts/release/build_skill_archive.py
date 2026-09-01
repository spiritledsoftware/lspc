#!/usr/bin/env python3
"""Build the standalone, deterministic companion-skill archive."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import stat
import time
import zipfile
from pathlib import Path

from build_archive import payload_files, sha256, skill_digest


ROOT = Path(__file__).resolve().parents[2]


def catalog_digest(schema_path: Path) -> str:
    value = json.loads(schema_path.read_text(encoding="utf-8"))
    catalog = value.get("result", value).get("catalog", value)
    if not isinstance(catalog, dict) or catalog.get("contractVersion") != 1:
        raise SystemExit("skill schema is not a v1 lspc catalog")
    bytes_ = json.dumps(catalog, sort_keys=True, separators=(",", ":")).encode()
    return f"sha256:{hashlib.sha256(bytes_).hexdigest()}"


def manifest(files: list[tuple[str, Path]], version: str, skill: str, schema: str) -> bytes:
    value = {
        "formatVersion": 1,
        "name": "lspc-agent-skill",
        "version": version,
        "skillDigest": skill,
        "schemaDigest": schema,
        "files": [
            {"path": f"skills/lspc/{relative}", "sha256": f"sha256:{sha256(path)}", "bytes": path.stat().st_size}
            for relative, path in files
        ],
    }
    return (json.dumps(value, sort_keys=True, separators=(",", ":")) + "\n").encode()


def write(path: Path, files: list[tuple[str, Path]], contents: bytes, prefix: str) -> None:
    epoch = max(315532800, int(os.environ.get("SOURCE_DATE_EPOCH", "0")))
    with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
        for relative, source in files:
            info = zipfile.ZipInfo(f"{prefix}/skills/lspc/{relative}", time.gmtime(epoch)[:6])
            info.external_attr = (stat.S_IFREG | 0o644) << 16
            archive.writestr(info, source.read_bytes(), compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
        info = zipfile.ZipInfo(f"{prefix}/manifest.json", time.gmtime(epoch)[:6])
        info.external_attr = (stat.S_IFREG | 0o644) << 16
        archive.writestr(info, contents, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--schema-json", required=True, type=Path)
    parser.add_argument("--skill-dir", type=Path, default=ROOT / "skills/lspc")
    parser.add_argument("--output-dir", required=True, type=Path)
    arguments = parser.parse_args()
    files = payload_files(arguments.skill_dir.resolve())
    contents = manifest(files, arguments.version, skill_digest(files), catalog_digest(arguments.schema_json))
    prefix = f"lspc-agent-skill-v{arguments.version}"
    arguments.output_dir.mkdir(parents=True, exist_ok=True)
    archive = arguments.output_dir / f"{prefix}.zip"
    write(archive, files, contents, prefix)
    with zipfile.ZipFile(archive) as verification:
        if verification.read(f"{prefix}/manifest.json") != contents or f"{prefix}/skills/lspc/SKILL.md" not in verification.namelist():
            raise SystemExit("companion skill archive verification failed")
    archive.with_suffix(".zip.sha256").write_text(f"{sha256(archive)}  {archive.name}\n", encoding="ascii")
    print(archive)


if __name__ == "__main__":
    main()
