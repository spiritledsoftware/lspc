#!/usr/bin/env python3
"""Build and verify a deterministic native release archive."""

from __future__ import annotations

import argparse
import gzip
import hashlib
import io
import json
import os
import stat
import tarfile
import time
import zipfile
from pathlib import Path, PurePosixPath


ROOT = Path(__file__).resolve().parents[2]


def sha256(path: Path) -> str:
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def payload_files(skill_dir: Path) -> list[tuple[str, Path]]:
    if not skill_dir.is_dir() or skill_dir.is_symlink():
        raise SystemExit(f"skill directory is missing or unsafe: {skill_dir}")
    files = []
    for path in skill_dir.rglob("*"):
        if path.is_symlink() or not path.is_file():
            if path.is_dir() and not path.is_symlink():
                continue
            raise SystemExit(f"skill payload contains a non-regular file: {path}")
        relative = path.relative_to(skill_dir).as_posix()
        if relative == ".lspctl-managed.json" or not relative or ".." in PurePosixPath(relative).parts:
            raise SystemExit(f"skill payload path is unsafe: {relative}")
        files.append((relative, path))
    if not files:
        raise SystemExit("skill payload is empty")
    return sorted(files)


def skill_digest(files: list[tuple[str, Path]]) -> str:
    digest = hashlib.sha256(b"lspctl-skill-v1\0" + len(files).to_bytes(8, "big"))
    for relative, path in files:
        encoded = relative.encode("utf-8")
        content = path.read_bytes()
        digest.update(len(encoded).to_bytes(8, "big"))
        digest.update(encoded)
        digest.update(len(content).to_bytes(8, "big"))
        digest.update(content)
    return f"sha256:{digest.hexdigest()}"


def archive_entries(binary: Path, skill_dir: Path) -> tuple[list[tuple[str, Path]], str]:
    files = [
        (f"lspctl{binary.suffix}", binary),
        ("README.md", ROOT / "README.md"),
        ("LICENSE-MIT", ROOT / "LICENSE-MIT"),
        ("LICENSE-APACHE", ROOT / "LICENSE-APACHE"),
        ("reference-servers.json", ROOT / "assets/reference-servers.json"),
    ]
    skill_files = payload_files(skill_dir)
    files.extend((f"skills/lspctl/{relative}", path) for relative, path in skill_files)
    if any(not path.is_file() for _, path in files):
        raise SystemExit("release payload is incomplete")
    return files, skill_digest(skill_files)


def archive_manifest(
    entries: list[tuple[str, Path]], version: str, target: str, commit: str, rust_version: str, skill: str
) -> bytes:
    reference_servers = json.loads(
        (ROOT / "assets/reference-servers.json").read_text(encoding="utf-8")
    )
    manifest = {
        "formatVersion": 1,
        "name": "lspctl",
        "version": version,
        "target": target,
        "sourceCommit": commit,
        "rustVersion": rust_version,
        "skillDigest": skill,
        "referenceServers": reference_servers,
        "files": [
            {"path": relative, "sha256": f"sha256:{sha256(path)}", "bytes": path.stat().st_size}
            for relative, path in entries
        ],
    }
    return (json.dumps(manifest, sort_keys=True, separators=(",", ":")) + "\n").encode()


def tar_info(name: str, data: bytes, mode: int, epoch: int) -> tarfile.TarInfo:
    info = tarfile.TarInfo(name)
    info.size, info.mode, info.mtime = len(data), mode, epoch
    info.uid, info.gid, info.uname, info.gname = 0, 0, "", ""
    return info


def write_archive(path: Path, entries: list[tuple[str, Path]], manifest: bytes, prefix: str, kind: str) -> None:
    epoch = max(315532800, int(os.environ.get("SOURCE_DATE_EPOCH", "0")))
    contents = [(relative, source.read_bytes(), 0o755 if relative.startswith("lspctl") else 0o644) for relative, source in entries]
    contents.append(("manifest.json", manifest, 0o644))
    if kind == "zip":
        with zipfile.ZipFile(path, "w", compression=zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
            timestamp = time.gmtime(epoch)[:6]
            for relative, data, mode in contents:
                info = zipfile.ZipInfo(f"{prefix}/{relative}", timestamp)
                info.external_attr = (stat.S_IFREG | mode) << 16
                archive.writestr(info, data, compress_type=zipfile.ZIP_DEFLATED, compresslevel=9)
        return
    with path.open("wb") as raw, gzip.GzipFile(filename="", mode="wb", fileobj=raw, mtime=epoch) as compressed:
        with tarfile.open(fileobj=compressed, mode="w") as archive:
            for relative, data, mode in contents:
                archive.addfile(tar_info(f"{prefix}/{relative}", data, mode, epoch), io.BytesIO(data))


def verify_archive(path: Path, manifest: bytes, prefix: str, kind: str) -> None:
    expected = {
        f"{prefix}/manifest.json",
        f"{prefix}/README.md",
        f"{prefix}/LICENSE-MIT",
        f"{prefix}/LICENSE-APACHE",
        f"{prefix}/reference-servers.json",
    }
    if kind == "zip":
        with zipfile.ZipFile(path) as archive:
            names = set(archive.namelist())
            actual = archive.read(f"{prefix}/manifest.json")
    else:
        with tarfile.open(path) as archive:
            names = set(archive.getnames())
            actual = archive.extractfile(f"{prefix}/manifest.json").read()
    if not expected <= names or not any(name.startswith(f"{prefix}/skills/lspctl/") for name in names):
        raise SystemExit("archive is missing required payload files")
    if actual != manifest:
        raise SystemExit("archive manifest changed while writing")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    parser.add_argument("--target", required=True)
    parser.add_argument("--version", required=True)
    parser.add_argument("--commit", required=True)
    parser.add_argument("--rust-version", required=True)
    parser.add_argument("--skill-dir", type=Path, default=ROOT / "skills/lspctl")
    parser.add_argument("--output-dir", type=Path, required=True)
    parser.add_argument("--format", choices=("auto", "tar.gz", "zip"), default="auto")
    arguments = parser.parse_args()
    binary = arguments.binary.resolve()
    if not binary.is_file() or binary.is_symlink():
        raise SystemExit(f"binary is missing or unsafe: {binary}")
    kind = "zip" if arguments.format == "zip" or (arguments.format == "auto" and "windows" in arguments.target) else "tar.gz"
    entries, skill = archive_entries(binary, arguments.skill_dir.resolve())
    manifest = archive_manifest(entries, arguments.version, arguments.target, arguments.commit, arguments.rust_version, skill)
    prefix = f"lspctl-v{arguments.version}-{arguments.target}"
    arguments.output_dir.mkdir(parents=True, exist_ok=True)
    archive = arguments.output_dir / f"{prefix}.{kind}"
    write_archive(archive, entries, manifest, prefix, kind)
    verify_archive(archive, manifest, prefix, kind)
    checksum = archive.with_suffix(archive.suffix + ".sha256")
    checksum.write_text(f"{sha256(archive)}  {archive.name}\n", encoding="ascii")
    print(archive)


if __name__ == "__main__":
    main()
