#!/usr/bin/env python3
"""Download one exact rust-analyzer release asset for the current host."""

from __future__ import annotations

import argparse
import gzip
import platform
import shutil
import stat
import time
import urllib.request
import zipfile
from pathlib import Path

TARGETS = {
    ("Linux", "x86_64"): "x86_64-unknown-linux-gnu.gz",
    ("Linux", "aarch64"): "aarch64-unknown-linux-gnu.gz",
    ("Darwin", "x86_64"): "x86_64-apple-darwin.gz",
    ("Darwin", "arm64"): "aarch64-apple-darwin.gz",
    ("Windows", "AMD64"): "x86_64-pc-windows-msvc.zip",
}


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--version", required=True)
    parser.add_argument("--output", required=True, type=Path)
    arguments = parser.parse_args()
    key = (platform.system(), platform.machine())
    suffix = TARGETS.get(key)
    if suffix is None:
        raise SystemExit(f"unsupported rust-analyzer test host: {key}")
    asset = f"rust-analyzer-{suffix}"
    url = f"https://github.com/rust-lang/rust-analyzer/releases/download/{arguments.version}/{asset}"
    archive = arguments.output.with_suffix(".download")
    arguments.output.parent.mkdir(parents=True, exist_ok=True)
    for attempt in range(3):
        try:
            urllib.request.urlretrieve(url, archive)
            break
        except OSError:
            if attempt == 2:
                raise
            time.sleep(2**attempt)
    if suffix.endswith(".gz"):
        with gzip.open(archive, "rb") as source, arguments.output.open("wb") as destination:
            shutil.copyfileobj(source, destination)
    else:
        with zipfile.ZipFile(archive) as source:
            member = next(name for name in source.namelist() if name.endswith("rust-analyzer.exe"))
            arguments.output.write_bytes(source.read(member))
    archive.unlink()
    arguments.output.chmod(arguments.output.stat().st_mode | stat.S_IXUSR)


if __name__ == "__main__":
    main()
