#!/usr/bin/env python3
"""Portable checks for deterministic release archive scaffolding."""

from __future__ import annotations

import hashlib
import json
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
ARCHIVE = ROOT / "scripts/release/build_archive.py"
SKILL_ARCHIVE = ROOT / "scripts/release/build_skill_archive.py"


class ReleaseArchiveTests(unittest.TestCase):
    def test_archives_contain_a_verified_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            temporary = Path(temporary)
            binary = temporary / "lspc"
            binary.write_bytes(b"production binary\n")
            skill = temporary / "skill"
            skill.mkdir()
            (skill / "SKILL.md").write_text("# lspc\n", encoding="utf-8")

            for target, extension in (("x86_64-unknown-linux-gnu", "tar.gz"), ("x86_64-pc-windows-msvc", "zip")):
                output = temporary / target
                subprocess.run(
                    [
                        sys.executable,
                        str(ARCHIVE),
                        "--binary",
                        str(binary),
                        "--target",
                        target,
                        "--version",
                        "1.2.3",
                        "--commit",
                        "0123456789abcdef",
                        "--rust-version",
                        "rustc 1.89.0",
                        "--skill-dir",
                        str(skill),
                        "--output-dir",
                        str(output),
                    ],
                    check=True,
                )
                archive = output / f"lspc-v1.2.3-{target}.{extension}"
                prefix = f"lspc-v1.2.3-{target}"
                if extension == "zip":
                    with zipfile.ZipFile(archive) as contents:
                        manifest = json.loads(contents.read(f"{prefix}/manifest.json"))
                        names = contents.namelist()
                else:
                    with tarfile.open(archive) as contents:
                        manifest = json.loads(contents.extractfile(f"{prefix}/manifest.json").read())
                        names = contents.getnames()
                self.assertIn(f"{prefix}/skills/lspc/SKILL.md", names)
                self.assertEqual(manifest["version"], "1.2.3")
                self.assertTrue(manifest["skillDigest"].startswith("sha256:"))
                checksum = (output / f"lspc-v1.2.3-{target}.{extension}.sha256").read_text(encoding="ascii")
                self.assertTrue(checksum.startswith(hashlib.sha256(archive.read_bytes()).hexdigest()))

    def test_companion_skill_archive_has_manifest_and_digests(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "output"
            subprocess.run([sys.executable, str(SKILL_ARCHIVE), "--version", "1.2.3", "--schema-json", str(ROOT / "assets/contract/catalog.json"), "--output-dir", str(output)], check=True)
            archive = output / "lspc-agent-skill-v1.2.3.zip"
            with zipfile.ZipFile(archive) as contents:
                manifest = json.loads(contents.read("lspc-agent-skill-v1.2.3/manifest.json"))
                self.assertIn("lspc-agent-skill-v1.2.3/skills/lspc/SKILL.md", contents.namelist())
            self.assertEqual(manifest["version"], "1.2.3")
            self.assertTrue(manifest["skillDigest"].startswith("sha256:"))
            self.assertTrue(manifest["schemaDigest"].startswith("sha256:"))


if __name__ == "__main__":
    unittest.main()
