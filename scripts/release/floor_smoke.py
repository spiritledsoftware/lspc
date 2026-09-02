#!/usr/bin/env python3
"""Exercise one published archive on a native minimum-floor host."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tarfile
import tempfile
import zipfile
from pathlib import Path, PurePosixPath

from smoke_environment import isolated_user_environment as test_environment


def safe_name(name: str) -> bool:
    path = PurePosixPath(name)
    return not path.is_absolute() and ".." not in path.parts


def extract(archive: Path, destination: Path) -> None:
    if archive.suffix == ".zip":
        with zipfile.ZipFile(archive) as source:
            if any(not safe_name(info.filename) or info.is_dir() for info in source.infolist()):
                raise SystemExit("release ZIP contains an unsafe or unexpected entry")
            source.extractall(destination)
        return
    with tarfile.open(archive) as source:
        members = source.getmembers()
        if any(not safe_name(member.name) or not member.isfile() for member in members):
            raise SystemExit("release tarball contains an unsafe or unexpected entry")
        for member in members:
            target = destination / member.name
            target.parent.mkdir(parents=True, exist_ok=True)
            extracted = source.extractfile(member)
            if extracted is None:
                raise SystemExit("release tarball entry cannot be read")
            target.write_bytes(extracted.read())
            target.chmod(member.mode & 0o777)


def command(
    binary: Path, env: dict[str, str], cwd: Path, *arguments: str
) -> dict[str, object]:
    completed = subprocess.run(
        [str(binary), *arguments], env=env, cwd=cwd, text=True, capture_output=True
    )
    if completed.stderr:
        raise SystemExit(f"floor command emitted stderr: {completed.stderr}")
    output = json.loads(completed.stdout)
    if completed.returncode != 0 or output.get("ok") is not True:
        raise SystemExit(f"floor command failed: {output!r}")
    return output


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--archive", type=Path, required=True)
    parser.add_argument("--fake-server", type=Path, required=True)
    parser.add_argument("--target", required=True)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    archive = arguments.archive.resolve()
    fake = arguments.fake_server.resolve()
    with tempfile.TemporaryDirectory(prefix="lspc-floor-") as temporary:
        root = Path(temporary)
        payload = root / "payload"
        payload.mkdir()
        extract(archive, payload)
        binary_name = "lspc.exe" if os.name == "nt" else "lspc"
        binaries = list(payload.glob(f"*/{binary_name}"))
        if len(binaries) != 1:
            raise SystemExit(f"archive has no unique production binary: {binaries!r}")
        binary = binaries[0]
        env = test_environment(root)
        workspace = root / "workspace-λ"
        workspace.mkdir()
        config = Path(env["XDG_CONFIG_HOME"]) / "lspc/config.toml"
        if os.name == "nt":
            config = Path(env["APPDATA"]) / "lspc/config.toml"
        elif __import__("platform").system() == "Darwin":
            config = Path(env["HOME"]) / "Library/Application Support/lspc/config.toml"
        config.parent.mkdir(parents=True)
        config.write_text(
            "version = 1\ndefault_server = \"fake\"\n[servers.fake]\n"
            + f"executable = {json.dumps(str(fake))}\n",
            encoding="utf-8",
        )

        schema = command(binary, env, workspace, "schema", "--full")
        first = command(
            binary,
            env,
            workspace,
            "raw",
            "--workspace",
            str(workspace),
            "--server",
            "fake",
            "--method",
            "fixture/floor-one",
        )
        second = command(
            binary,
            env,
            workspace,
            "raw",
            "--workspace",
            str(workspace),
            "--server",
            "fake",
            "--method",
            "fixture/floor-two",
        )
        if first["context"]["ownerGeneration"] != second["context"]["ownerGeneration"]:
            raise SystemExit("floor smoke did not reuse its Owner")
        local_skill = command(binary, env, workspace, "skill", "install")
        global_skill = command(binary, env, workspace, "skill", "install", "--global")
        stopped = command(
            binary,
            env,
            workspace,
            "session",
            "stop",
            "--workspace",
            str(workspace),
            "--server",
            "fake",
        )
        report = {
            "formatVersion": 1,
            "target": arguments.target,
            "schemaVersion": schema["result"]["contractVersion"],
            "ownerGeneration": first["context"]["ownerGeneration"],
            "localSkillOutcome": local_skill["result"]["outcome"],
            "globalSkillOutcome": global_skill["result"]["outcome"],
            "stopOutcome": stopped["result"]["outcome"],
        }
        arguments.output.parent.mkdir(parents=True, exist_ok=True)
        arguments.output.write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
