#!/usr/bin/env python3
"""Install and run one exact reference-server smoke cell."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import venv
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
REFERENCE_VERSIONS = json.loads(
    (ROOT / "assets/reference-servers.json").read_text(encoding="utf-8")
)
RUST_ANALYZER = REFERENCE_VERSIONS["rustAnalyzer"]["version"]
TYPESCRIPT_LANGUAGE_SERVER = REFERENCE_VERSIONS["typescriptLanguageServer"]["version"]
TYPESCRIPT = REFERENCE_VERSIONS["typescriptLanguageServer"]["typescriptVersion"]
BASEDPYRIGHT = REFERENCE_VERSIONS["basedpyright"]["version"]


def executable(directory: Path, name: str) -> Path:
    suffix = ".exe" if os.name == "nt" else ""
    return directory / f"{name}{suffix}"


def native(path: Path) -> Path:
    if os.name == "nt" and path.suffix.lower() != ".exe":
        return path.with_suffix(".exe")
    return path


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("server", choices=["rust-analyzer", "typescript-language-server", "basedpyright"])
    parser.add_argument("--binary", type=Path, default=ROOT / "target" / "debug" / ("lspctl.exe" if os.name == "nt" else "lspctl"))
    parser.add_argument("--tools", type=Path, default=ROOT / ".reference-tools")
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    tools = arguments.tools.resolve()
    tools.mkdir(parents=True, exist_ok=True)
    binary = native(arguments.binary.resolve())
    output = arguments.output or ROOT / f"reference-{arguments.server}.json"
    smoke = [
        sys.executable,
        str(ROOT / "scripts/release/reference_smoke.py"),
        "--binary",
        str(binary),
        "--require-schema-validation",
        "--output",
        str(output),
    ]

    if arguments.server == "rust-analyzer":
        server = executable(tools, "rust-analyzer")
        subprocess.run([
            sys.executable, str(ROOT / "scripts/release/install_rust_analyzer.py"),
            "--version", RUST_ANALYZER, "--output", str(server),
        ], check=True)
        command = [*smoke, "--server", "rust", "--executable", str(server), "--language-id", "rust", "--extension", ".rs", "--fixture", "rust"]
    elif arguments.server == "typescript-language-server":
        node = tools / "node"
        npm = "npm.cmd" if os.name == "nt" else "npm"
        subprocess.run([
            npm, "install", "--prefix", str(node),
            f"typescript-language-server@{TYPESCRIPT_LANGUAGE_SERVER}", f"typescript@{TYPESCRIPT}",
        ], check=True)
        node_executable = shutil.which("node")
        if node_executable is None:
            raise SystemExit("node is unavailable after npm installation")
        cli = node / "node_modules" / "typescript-language-server" / "lib" / "cli.mjs"
        if not cli.is_file():
            raise SystemExit(f"typescript-language-server CLI is missing: {cli}")
        command = [*smoke, "--server", "typescript", "--executable", node_executable, f"--server-arg={cli}", "--server-arg=--stdio", "--language-id", "typescript", "--extension", ".ts", "--fixture", "typescript"]
    else:
        environment = tools / "basedpyright"
        venv.EnvBuilder(with_pip=True, clear=True).create(environment)
        scripts = environment / ("Scripts" if os.name == "nt" else "bin")
        python = executable(scripts, "python")
        subprocess.run([str(python), "-m", "pip", "install", f"basedpyright=={BASEDPYRIGHT}", "jsonschema==4.25.1"], check=True)
        server = executable(scripts, "basedpyright-langserver")
        command = [str(python), str(ROOT / "scripts/release/reference_smoke.py"), "--binary", str(binary), "--require-schema-validation", "--output", str(output), "--server", "basedpyright", "--executable", str(server), "--server-arg=--stdio", "--language-id", "python", "--extension", ".py", "--fixture", "python"]
    subprocess.run(command, check=True)


if __name__ == "__main__":
    main()
