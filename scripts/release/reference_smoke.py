#!/usr/bin/env python3
"""Run the frozen reference-language-server smoke contract."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import tempfile
import time
from pathlib import Path


def native(path: Path) -> Path:
    if os.name == "nt" and path.suffix.lower() != ".exe":
        return path.with_suffix(".exe")
    return path


def environment(root: Path) -> dict[str, str]:
    values = os.environ.copy()
    home = root / "home"
    values["HOME"] = str(home)
    values["USERPROFILE"] = str(home)
    values["XDG_CONFIG_HOME"] = str(root / "config")
    values["XDG_STATE_HOME"] = str(root / "state")
    values["APPDATA"] = str(root / "roaming")
    values["LOCALAPPDATA"] = str(root / "local")
    return values


def toml_string(value: str) -> str:
    return json.dumps(value)


class Smoke:
    def __init__(self, binary: Path, workspace: Path, env: dict[str, str]) -> None:
        self.binary = binary
        self.workspace = workspace
        self.env = env
        self.outputs: list[dict[str, object]] = []

    def run(self, *arguments: str) -> dict[str, object]:
        command = [str(self.binary), *arguments]
        completed = subprocess.run(command, env=self.env, check=False, capture_output=True)
        if completed.stderr:
            raise SystemExit(f"command wrote stderr: {command!r}: {completed.stderr.decode(errors='replace')}")
        try:
            output = json.loads(completed.stdout)
        except (UnicodeDecodeError, json.JSONDecodeError) as error:
            raise SystemExit(f"command did not emit JSON: {command!r}: {error}") from error
        if completed.returncode != 0 or output.get("ok") is not True:
            raise SystemExit(f"command failed: {command!r}: {json.dumps(output, sort_keys=True)}")
        self.outputs.append(output)
        return output


def fixture(kind: str, workspace: Path) -> tuple[Path, int, int, str]:
    if kind == "rust":
        (workspace / "Cargo.toml").write_text(
            '[package]\nname = "lspc-reference"\nversion = "0.1.0"\nedition = "2024"\n',
            encoding="utf-8",
        )
        source = workspace / "src/main.rs"
        source.parent.mkdir()
        source.write_text("fn target() -> i32 { 1 }\nfn main() { let _ = target(); }\n", encoding="utf-8")
        return source, 1, 22, "renamed_target"
    if kind == "typescript":
        (workspace / "tsconfig.json").write_text(
            '{"compilerOptions":{"strict":true,"noEmit":true},"include":["main.ts"]}\n',
            encoding="utf-8",
        )
        source = workspace / "main.ts"
        source.write_text("function target(): number { return 1; }\nconst value = target();\n", encoding="utf-8")
        return source, 1, 16, "renamedTarget"
    if kind == "python":
        (workspace / "pyrightconfig.json").write_text(
            '{"typeCheckingMode":"strict","include":["main.py"]}\n', encoding="utf-8"
        )
        source = workspace / "main.py"
        source.write_text("def target() -> int:\n    return 1\n\nvalue = target()\n", encoding="utf-8")
        return source, 3, 10, "renamed_target"
    raise SystemExit(f"unknown fixture kind: {kind}")


def validate_outputs(
    binary: Path, outputs: list[dict[str, object]], env: dict[str, str]
) -> bool:
    try:
        from jsonschema import Draft202012Validator
        from referencing import Registry, Resource
    except ImportError:
        return False
    schema_output = json.loads(
        subprocess.run(
            [str(binary), "schema", "--full"], env=env, check=True, capture_output=True
        ).stdout
    )
    schemas = schema_output["result"]["schemas"]
    registry = Registry().with_resources(
        (uri, Resource.from_contents(schema)) for uri, schema in schemas.items()
    )
    for output in outputs:
        command = "/".join(output["command"])
        suffix = "success" if output["ok"] else "failure"
        uri = f"lspc://schema/v1/command/{command}/{suffix}"
        Draft202012Validator(schemas[uri], registry=registry).validate(output)
    return True


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--server", required=True)
    parser.add_argument("--executable", type=Path, required=True)
    parser.add_argument("--server-arg", action="append", default=[])
    parser.add_argument("--language-id", required=True)
    parser.add_argument("--extension", required=True)
    parser.add_argument("--fixture", choices=["rust", "typescript", "python"], required=True)
    parser.add_argument("--require-schema-validation", action="store_true")
    parser.add_argument("--output", type=Path)
    arguments = parser.parse_args()
    arguments.binary = native(arguments.binary.resolve())

    with tempfile.TemporaryDirectory(prefix=f"lspc-reference-{arguments.server}-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        source, line, column, new_name = fixture(arguments.fixture, workspace)
        config = [
            "version = 1",
            f"default_server = {toml_string(arguments.server)}",
            "routes = [{ server = " + toml_string(arguments.server)
            + ", language_id = " + toml_string(arguments.language_id)
            + ", extensions = [" + toml_string(arguments.extension) + "] }]",
            f"[servers.{arguments.server}]",
            f"executable = {toml_string(str(arguments.executable.resolve()))}",
            "args = " + json.dumps(arguments.server_arg),
        ]
        (workspace / ".lspc.toml").write_text("\n".join(config) + "\n", encoding="utf-8")
        env = environment(root)
        smoke = Smoke(arguments.binary, workspace, env)
        workspace_text = str(workspace)
        status = smoke.run("trust", "status", "--workspace", workspace_text, "--server", arguments.server)
        digest = status["result"]["records"][0]["currentDigest"]
        smoke.run(
            "trust", "grant", "--workspace", workspace_text, "--server", arguments.server,
            "--digest", digest,
        )
        first = smoke.run("capabilities", "--workspace", workspace_text, "--server", arguments.server)
        second = smoke.run("capabilities", "--workspace", workspace_text, "--server", arguments.server)
        if first["context"]["ownerGeneration"] != second["context"]["ownerGeneration"]:
            raise SystemExit("reference server Owner was not reused")
        definition = smoke.run(
            "definition", "--workspace", workspace_text, "--server", arguments.server,
            "--file", str(source), "--line", str(line), "--column", str(column),
            "--request-timeout", "2m",
        )
        if definition.get("result") is None:
            raise SystemExit("reference definition returned null")
        time.sleep(2)
        smoke.run(
            "published-diagnostics", "--workspace", workspace_text, "--server", arguments.server,
            "--file", str(source),
        )
        preview = smoke.run(
            "rename", "--workspace", workspace_text, "--server", arguments.server,
            "--file", str(source), "--line", str(line), "--column", str(column),
            "--new-name", new_name, "--request-timeout", "2m",
        )
        if preview.get("outcome") != "previewed":
            raise SystemExit(f"reference rename did not create a Preview: {preview!r}")
        preview_id = preview["result"]["previewId"]
        inspected = smoke.run("preview", "show", preview_id)
        if inspected["result"]["previewId"] != preview_id:
            raise SystemExit("reference Preview inspection returned a different Preview")
        applied = smoke.run("apply", preview_id)
        if applied.get("outcome") != "applied" or new_name not in source.read_text(encoding="utf-8"):
            raise SystemExit("reference rename Preview was not applied")
        receipt_id = applied["result"]["receiptId"]
        receipt = smoke.run("receipt", "show", receipt_id)
        if receipt["result"]["outcome"] != "applied":
            raise SystemExit("reference Receipt did not record an applied outcome")
        stopped = smoke.run(
            "session", "stop", "--workspace", workspace_text, "--server", arguments.server
        )
        if stopped["result"]["outcome"] != "stopped":
            raise SystemExit("reference Owner did not stop gracefully")
        schema_validated = validate_outputs(arguments.binary, smoke.outputs, env)
        if arguments.require_schema_validation and not schema_validated:
            raise SystemExit("jsonschema and referencing are required for this acceptance cell")
        report = {
            "formatVersion": 1,
            "server": arguments.server,
            "ownerGeneration": first["context"]["ownerGeneration"],
            "commands": len(smoke.outputs),
            "schemaValidated": schema_validated,
        }
        if arguments.output:
            arguments.output.parent.mkdir(parents=True, exist_ok=True)
            arguments.output.write_text(
                json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
            )
        print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
