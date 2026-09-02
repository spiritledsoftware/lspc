#!/usr/bin/env python3
"""Audit the declared runtime floor encoded in one native release binary."""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from pathlib import Path


WINDOWS_DLLS = {
    "advapi32.dll",
    "bcrypt.dll",
    "bcryptprimitives.dll",
    "combase.dll",
    "kernel32.dll",
    "ntdll.dll",
    "ole32.dll",
    "shell32.dll",
    "userenv.dll",
    "ws2_32.dll",
}


def version(value: str) -> tuple[int, ...]:
    return tuple(int(component) for component in value.split("."))


def linux_floor(output: str) -> dict[str, object]:
    versions = sorted(
        {match.group(1) for match in re.finditer(r"\bGLIBC_(\d+(?:\.\d+)+)\b", output)},
        key=version,
    )
    if not versions:
        raise SystemExit("ELF binary has no auditable GLIBC symbol requirements")
    maximum = versions[-1]
    if version(maximum) > version("2.28"):
        raise SystemExit(f"ELF binary requires GLIBC_{maximum}, above the 2.28 floor")
    return {"platform": "linux", "requiredGlibc": maximum, "floor": "2.28"}


def macos_floor(output: str) -> dict[str, object]:
    versions = re.findall(r"^\s+minos\s+(\d+(?:\.\d+)+)\s*$", output, re.MULTILINE)
    if not versions:
        versions = re.findall(
            r"^\s+cmd LC_VERSION_MIN_MACOSX\s*\n\s+cmdsize \d+\s*\n\s+version (\d+(?:\.\d+)+)\s*$",
            output,
            re.MULTILINE,
        )
    if not versions:
        raise SystemExit("Mach-O binary has no auditable minimum macOS load command")
    minimum = max(versions, key=version)
    if version(minimum) > version("12.0"):
        raise SystemExit(f"Mach-O minimum macOS {minimum} is above the 12.0 floor")
    return {"platform": "macos", "minimumOs": minimum, "floor": "12.0"}


def windows_floor(dependents: str, headers: str) -> dict[str, object]:
    dlls = sorted(
        {
            match.group(1).lower()
            for match in re.finditer(r"^\s+([A-Za-z0-9_.-]+\.dll)\s*$", dependents, re.MULTILINE)
        }
    )
    if not dlls:
        raise SystemExit("PE binary has no auditable import table")
    unsupported = [
        dll
        for dll in dlls
        if dll not in WINDOWS_DLLS and not dll.startswith("api-ms-win-")
    ]
    if unsupported:
        raise SystemExit(f"PE binary imports unreviewed runtime DLLs: {unsupported!r}")
    matches = re.findall(
        r"^\s*(\d+(?:\.\d+)?)\s+(?:operating system|subsystem) version\s*$",
        headers,
        re.MULTILINE | re.IGNORECASE,
    )
    if not matches:
        raise SystemExit("PE binary has no auditable OS/subsystem version")
    encoded = max(matches, key=version)
    if version(encoded) > version("10.0"):
        raise SystemExit(f"PE encoded OS version {encoded} is above Windows 10")
    return {
        "platform": "windows",
        "encodedOsVersion": encoded,
        "floor": "Windows 10 1809 / Server 2019",
        "imports": dlls,
    }


def checked_output(arguments: list[str]) -> str:
    return subprocess.run(arguments, check=True, text=True, capture_output=True).stdout


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    binary = arguments.binary.resolve()
    if not binary.is_file() or binary.is_symlink():
        raise SystemExit(f"release binary is missing or unsafe: {binary}")

    if "linux" in arguments.target:
        report = linux_floor(checked_output(["readelf", "--version-info", str(binary)]))
    elif "apple-darwin" in arguments.target:
        report = macos_floor(checked_output(["otool", "-l", str(binary)]))
    elif "windows-msvc" in arguments.target:
        report = windows_floor(
            checked_output(["dumpbin", "/nologo", "/dependents", str(binary)]),
            checked_output(["dumpbin", "/nologo", "/headers", str(binary)]),
        )
    else:
        raise SystemExit(f"unsupported release target: {arguments.target}")

    report.update({"formatVersion": 1, "target": arguments.target})
    rendered = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if arguments.output:
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(rendered, encoding="utf-8")
    print(rendered, end="")


if __name__ == "__main__":
    main()
