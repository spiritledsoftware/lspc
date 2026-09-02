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
import importlib.util
from pathlib import Path
from unittest import mock


ROOT = Path(__file__).resolve().parents[1]
ARCHIVE = ROOT / "scripts/release/build_archive.py"
SKILL_ARCHIVE = ROOT / "scripts/release/build_skill_archive.py"
VERIFY_ARCHIVE = ROOT / "scripts/release/verify_archive.py"
FLOOR_AUDIT = ROOT / "scripts/release/audit_runtime_floor.py"
PUBLISH_RELEASE = ROOT / "scripts/release/publish_release.py"
REFERENCE_SMOKE = ROOT / "scripts/release/reference_smoke.py"
SOAK = ROOT / "scripts/release/soak.py"


def load_script(path: Path):
    spec = importlib.util.spec_from_file_location(path.stem, path)
    module = importlib.util.module_from_spec(spec)
    assert spec.loader is not None
    sys.path.insert(0, str(path.parent))
    try:
        spec.loader.exec_module(module)
    finally:
        sys.path.pop(0)
    return module


class ReleaseArchiveTests(unittest.TestCase):
    def test_publish_commands_do_not_require_captured_stdout(self) -> None:
        publish = load_script(PUBLISH_RELEASE)
        completed = subprocess.CompletedProcess(["gh"], 0)
        with mock.patch.object(publish.subprocess, "run", return_value=completed):
            self.assertIsNone(publish.run(["gh"]))

    def test_runtime_floor_parsers_reject_newer_requirements(self) -> None:
        audit = load_script(FLOOR_AUDIT)
        self.assertEqual(
            audit.linux_floor("Version: GLIBC_2.17 GLIBC_2.28")["requiredGlibc"],
            "2.28",
        )
        with self.assertRaises(SystemExit):
            audit.linux_floor("Version: GLIBC_2.34")
        self.assertEqual(
            audit.macos_floor(
                "cmd LC_BUILD_VERSION\n  minos 12.0\n      sdk 15.5\n     tool LD\n  version 1167.5\n"
            )["minimumOs"],
            "12.0",
        )
        self.assertEqual(
            audit.macos_floor(
                "      cmd LC_VERSION_MIN_MACOSX\n  cmdsize 16\n  version 10.15\n      sdk 11.0\n"
            )["minimumOs"],
            "10.15",
        )
        with self.assertRaises(SystemExit):
            audit.macos_floor("cmd LC_BUILD_VERSION\n  minos 13.0\n")
        dependents = "    KERNEL32.dll\n    bcryptprimitives.dll\n    combase.dll\n"
        headers = "  10.00 operating system version\n  6.00 subsystem version\n"
        self.assertEqual(audit.windows_floor(dependents, headers)["encodedOsVersion"], "10.00")
        with self.assertRaises(SystemExit):
            audit.windows_floor("    future.dll\n", headers)
        with self.assertRaises(SystemExit):
            audit.windows_floor("    VCRUNTIME140.dll\n", headers)

    def test_release_smoke_environments_create_isolated_user_directories(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for script, function in ((REFERENCE_SMOKE, "environment"), (SOAK, "test_environment")):
                environment = getattr(load_script(script), function)(root / script.stem)
                for name in ("HOME", "XDG_CONFIG_HOME", "XDG_STATE_HOME", "APPDATA", "LOCALAPPDATA"):
                    self.assertTrue(Path(environment[name]).is_dir())

            workspace = root / "workspace"
            workspace.mkdir()
            _, line, column, _ = load_script(REFERENCE_SMOKE).fixture("rust", workspace)
            self.assertEqual((line, column), (1, 22))

    def test_reference_smoke_preserves_rust_toolchain_home(self) -> None:
        reference = load_script(REFERENCE_SMOKE)
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            toolchain_home = root / "toolchain-home"
            with mock.patch.dict(
                reference.os.environ,
                {"HOME": str(toolchain_home), "USERPROFILE": str(toolchain_home)},
                clear=True,
            ):
                environment = reference.environment(root / "isolated")
            self.assertEqual(Path(environment["CARGO_HOME"]), toolchain_home / ".cargo")
            self.assertEqual(Path(environment["RUSTUP_HOME"]), toolchain_home / ".rustup")

    def test_reference_smoke_allows_transient_content_modified(self) -> None:
        reference = load_script(REFERENCE_SMOKE)
        failure = subprocess.CompletedProcess(
            ["lspc", "definition"],
            5,
            stdout=b'{"ok":false,"error":{"code":"content_modified"}}',
            stderr=b"",
        )
        smoke = reference.Smoke(Path("lspc"), Path("workspace"), {})
        with mock.patch.object(reference.subprocess, "run", return_value=failure):
            output = smoke.run("definition", allowed_error_code="content_modified")
        self.assertEqual(output["error"]["code"], "content_modified")
        self.assertEqual(smoke.outputs, [])

    def test_soak_uses_the_native_user_config_path(self) -> None:
        soak = load_script(SOAK)
        with tempfile.TemporaryDirectory() as temporary:
            environment = soak.test_environment(Path(temporary))
            expected = {
                "Linux": Path(environment["XDG_CONFIG_HOME"]) / "lspc/config.toml",
                "Darwin": Path(environment["HOME"]) / "Library/Application Support/lspc/config.toml",
                "Windows": Path(environment["APPDATA"]) / "lspc/config.toml",
            }
            for system, path in expected.items():
                with mock.patch.object(soak.platform, "system", return_value=system):
                    self.assertEqual(soak.soak_user_config_path(environment), path)

    def test_archives_contain_a_verified_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            temporary = Path(temporary)
            skill = temporary / "skill"
            skill.mkdir()
            (skill / "SKILL.md").write_text("# lspc\n", encoding="utf-8")

            for target, extension in (("x86_64-unknown-linux-gnu", "tar.gz"), ("x86_64-pc-windows-msvc", "zip")):
                binary = temporary / ("lspc.exe" if "windows" in target else "lspc")
                binary.write_bytes(b"production binary\n")
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
                self.assertEqual(
                    manifest["referenceServers"]["rustAnalyzer"]["version"],
                    "2026-08-31",
                )
                checksum = (output / f"lspc-v1.2.3-{target}.{extension}.sha256").read_text(encoding="ascii")
                self.assertTrue(checksum.startswith(hashlib.sha256(archive.read_bytes()).hexdigest()))
                subprocess.run([sys.executable, str(VERIFY_ARCHIVE), str(archive), "--target", target, "--version", "1.2.3"], check=True)

    def test_archives_are_reproducible_and_reject_tampered_payloads(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            temporary = Path(temporary)
            binary = temporary / "lspc"
            binary.write_bytes(b"production binary\n")
            skill = temporary / "skill"
            skill.mkdir()
            (skill / "SKILL.md").write_text("# lspc\n", encoding="utf-8")
            base = [sys.executable, str(ARCHIVE), "--binary", str(binary), "--target", "x86_64-unknown-linux-gnu", "--version", "1.2.3", "--commit", "0123456789abcdef", "--rust-version", "rustc 1.89.0", "--skill-dir", str(skill)]
            first, second = temporary / "first", temporary / "second"
            subprocess.run([*base, "--output-dir", str(first)], check=True, env={**__import__("os").environ, "SOURCE_DATE_EPOCH": "315532800"})
            subprocess.run([*base, "--output-dir", str(second)], check=True, env={**__import__("os").environ, "SOURCE_DATE_EPOCH": "315532800"})
            archive = first / "lspc-v1.2.3-x86_64-unknown-linux-gnu.tar.gz"
            self.assertEqual(archive.read_bytes(), (second / archive.name).read_bytes())
            checksum = archive.with_suffix(".gz.sha256")
            checksum.write_text("0" * 64 + f"  {archive.name}\n", encoding="ascii")
            result = subprocess.run([sys.executable, str(VERIFY_ARCHIVE), str(archive), "--target", "x86_64-unknown-linux-gnu", "--version", "1.2.3"], text=True, capture_output=True)
            self.assertNotEqual(result.returncode, 0)

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
