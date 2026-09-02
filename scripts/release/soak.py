#!/usr/bin/env python3
"""Run the deterministic Owner reuse, performance, and resource soak."""

from __future__ import annotations

import argparse
import json
import os
import platform
import subprocess
import tempfile
import time
from pathlib import Path


def native(path: Path) -> Path:
    if os.name == "nt" and path.suffix.lower() != ".exe":
        return path.with_suffix(".exe")
    return path


def test_environment(root: Path) -> dict[str, str]:
    env = os.environ.copy()
    home = root / "home"
    roaming = home / "AppData/Roaming"
    local = home / "AppData/Local"
    for directory in (home, root / "config", root / "state", roaming, local):
        directory.mkdir(parents=True, exist_ok=True)
    env.update({
        "HOME": str(home),
        "USERPROFILE": str(home),
        "XDG_CONFIG_HOME": str(root / "config"),
        "XDG_STATE_HOME": str(root / "state"),
        "APPDATA": str(roaming),
        "LOCALAPPDATA": str(local),
    })
    return env


def soak_user_config_path(env: dict[str, str]) -> Path:
    system = platform.system()
    if system == "Windows":
        return Path(env["APPDATA"]) / "lspc/config.toml"
    if system == "Darwin":
        return Path(env["HOME"]) / "Library/Application Support/lspc/config.toml"
    return Path(env["XDG_CONFIG_HOME"]) / "lspc/config.toml"


def endpoint(root: Path) -> dict[str, object]:
    matches = list(root.glob("**/owners/endpoints/*.json"))
    if len(matches) != 1:
        raise SystemExit(f"expected one Owner endpoint, found {matches!r}")
    return json.loads(matches[0].read_text(encoding="utf-8"))


def command(binary: Path, env: dict[str, str], *arguments: str) -> tuple[dict[str, object], float]:
    started = time.perf_counter()
    result = subprocess.run([str(binary), *arguments], env=env, capture_output=True)
    elapsed_ms = (time.perf_counter() - started) * 1000
    if result.stderr:
        raise SystemExit(f"lspc emitted stderr: {result.stderr.decode(errors='replace')}")
    output = json.loads(result.stdout)
    if result.returncode != 0 or output.get("ok") is not True:
        raise SystemExit(f"lspc command failed: {output!r}")
    return output, elapsed_ms


def unix_descendants(pid: int) -> list[int]:
    if platform.system() == "Linux":
        found: list[int] = []
        pending = [pid]
        while pending:
            parent = pending.pop()
            path = Path(f"/proc/{parent}/task/{parent}/children")
            children = [int(value) for value in path.read_text().split()] if path.exists() else []
            found.extend(children)
            pending.extend(children)
        return found
    output = subprocess.run(["pgrep", "-P", str(pid)], capture_output=True, text=True)
    return [int(value) for value in output.stdout.split()]


def windows_descendants(pid: int) -> list[int]:
    script = (
        "$all=Get-CimInstance Win32_Process; $pending=@(" + str(pid) + "); $out=@(); "
        "while($pending.Count -gt 0){$p=$pending[0];$pending=$pending[1..($pending.Count-1)];"
        "$c=@($all|Where-Object ParentProcessId -eq $p|ForEach-Object ProcessId);"
        "$out+=$c;$pending+=$c};$out -join ' '"
    )
    output = subprocess.run(["powershell", "-NoProfile", "-Command", script], capture_output=True, text=True, check=True)
    return [int(value) for value in output.stdout.split()]


def process_metrics(pid: int) -> dict[str, int]:
    if platform.system() == "Linux":
        status = Path(f"/proc/{pid}/status").read_text()
        rss_kib = int(next(line.split()[1] for line in status.splitlines() if line.startswith("VmRSS:")))
        handles = len(list(Path(f"/proc/{pid}/fd").iterdir()))
        descendants = unix_descendants(pid)
        return {"memoryBytes": rss_kib * 1024, "handles": handles, "descendants": len(descendants)}
    if platform.system() == "Darwin":
        rss_kib = int(subprocess.check_output(["ps", "-o", "rss=", "-p", str(pid)], text=True).strip())
        handles = len([line for line in subprocess.check_output(["lsof", "-a", "-p", str(pid), "-Fn"], text=True).splitlines() if line.startswith("f")])
        return {"memoryBytes": rss_kib * 1024, "handles": handles, "descendants": len(unix_descendants(pid))}
    # PowerShell uses the same native counters as GetProcessMemoryInfo and GetProcessHandleCount.
    script = f"$p=Get-Process -Id {pid}; '{{\"memoryBytes\":'+$p.PrivateMemorySize64+',\"handles\":'+$p.HandleCount+'}}'"
    metrics = json.loads(subprocess.check_output(["powershell", "-NoProfile", "-Command", script], text=True))
    metrics["descendants"] = len(windows_descendants(pid))
    return metrics


def process_alive(pid: int) -> bool:
    if platform.system() == "Linux":
        return Path(f"/proc/{pid}").exists()
    if os.name == "nt":
        script = f"if (Get-Process -Id {pid} -ErrorAction SilentlyContinue) {{ exit 0 }} else {{ exit 1 }}"
        return subprocess.run(
            ["powershell", "-NoProfile", "-Command", script], capture_output=True
        ).returncode == 0
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True
    return True


def percentile95(values: list[float]) -> float:
    ordered = sorted(values)
    return ordered[max(0, min(len(ordered) - 1, int(len(ordered) * 0.95) - 1))]


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--fake-server", type=Path, required=True)
    parser.add_argument("--queries", type=int, default=1000)
    parser.add_argument("--output", type=Path, required=True)
    arguments = parser.parse_args()
    binary = native(arguments.binary.resolve())
    fake = native(arguments.fake_server.resolve())
    if arguments.queries < 2:
        raise SystemExit("soak needs at least two measured Queries")

    with tempfile.TemporaryDirectory(prefix="lspc-soak-") as temporary:
        root = Path(temporary)
        workspace = root / "workspace"
        workspace.mkdir()
        env = test_environment(root)
        config = soak_user_config_path(env)
        config.parent.mkdir(parents=True)
        config.write_text(
            "version = 1\ndefault_server = \"fake\"\n"
            'routes = [{ server = "fake", language_id = "rust", extensions = [".rs"] }]\n'
            "[synchronization]\n"
            "max_open_documents = 4\n"
            "max_document_bytes = 1048576\n"
            "max_total_text_bytes = 4194304\n"
            "max_diagnostic_snapshots = 4\n"
            "max_diagnostic_bytes = 4096\n"
            "[servers.fake]\n"
            + f"executable = {json.dumps(str(fake))}\n"
            + 'published_diagnostics_wait = "1ms"\n',
            encoding="utf-8",
        )
        common = ("--workspace", str(workspace), "--server", "fake", "--method", "fixture/soak")
        cold, cold_ms = command(binary, env, "raw", *common)
        generation = cold["context"]["ownerGeneration"]
        timings: list[float] = []
        for _ in range(100):
            output, elapsed = command(binary, env, "raw", *common)
            if output["context"]["ownerGeneration"] != generation:
                raise SystemExit("Owner generation changed during warmup")
            timings.append(elapsed)
        owner_pid = int(endpoint(root)["ownerPid"])
        baseline = process_metrics(owner_pid)
        samples: dict[str, dict[str, int]] = {}
        midpoint = arguments.queries // 2
        for index in range(1, arguments.queries + 1):
            output, elapsed = command(binary, env, "raw", *common)
            if output["context"]["ownerGeneration"] != generation:
                raise SystemExit("Owner generation changed during soak")
            timings.append(elapsed)
            if index in (midpoint, arguments.queries):
                samples[str(index)] = process_metrics(owner_pid)

        open_document_count = 0
        for index in range(12):
            document = workspace / f"churn-{index}.rs"
            document.write_text(f"fn churn_{index}() {{}}\n", encoding="utf-8")
            opened, _ = command(
                binary,
                env,
                "raw",
                "--workspace",
                str(workspace),
                "--server",
                "fake",
                "--method",
                "test/open-documents",
                "--sync-file",
                str(document),
            )
            open_document_count = int(opened["result"]["count"])
            if open_document_count > 4:
                raise SystemExit("Document churn exceeded max_open_documents")
        if open_document_count != 4:
            raise SystemExit("Document churn did not exercise bounded eviction")

        for index in range(12):
            uri = (workspace / f"diagnostic-{index}.rs").resolve().as_uri()
            command(
                binary,
                env,
                "raw",
                "--workspace",
                str(workspace),
                "--server",
                "fake",
                "--method",
                "test/publish-diagnostic",
                "--params-json",
                json.dumps({"uri": uri}, separators=(",", ":")),
            )
        published, _ = command(
            binary,
            env,
            "published-diagnostics",
            "--workspace",
            str(workspace),
            "--server",
            "fake",
            "--all-known",
            "--limit",
            "1000",
        )
        diagnostic_snapshot_count = len(published["result"])
        if diagnostic_snapshot_count != 4:
            raise SystemExit(
                "Diagnostic churn did not retain exactly max_diagnostic_snapshots: "
                f"{diagnostic_snapshot_count}"
            )

        time.sleep(5)
        final = process_metrics(owner_pid)
        status, _ = command(binary, env, "session", "status", "--workspace", str(workspace), "--server", "fake")
        if status["result"]["queueDepth"] != 0:
            raise SystemExit("Owner queue did not quiesce")
        if baseline["descendants"] != 1 or final["descendants"] != 1:
            raise SystemExit(f"Owner process tree is not exactly one server child: {baseline!r} -> {final!r}")
        if final["memoryBytes"] - baseline["memoryBytes"] > 32 * 1024 * 1024:
            raise SystemExit("Owner memory exceeded 32 MiB warm-baseline growth")
        middle = samples[str(midpoint)]
        end = samples[str(arguments.queries)]
        if end["memoryBytes"] - middle["memoryBytes"] > 8 * 1024 * 1024:
            raise SystemExit("Owner memory grew by more than 8 MiB in the second soak half")
        if final["handles"] - baseline["handles"] > 4:
            raise SystemExit("Owner handle/file-descriptor growth exceeded four")
        p95 = percentile95(timings[-100:])
        if p95 >= 250:
            raise SystemExit(f"warm Query p95 exceeded 250 ms: {p95:.2f} ms")
        if cold_ms >= 2000:
            raise SystemExit(f"cold readiness exceeded two seconds: {cold_ms:.2f} ms")
        command(binary, env, "session", "stop", "--workspace", str(workspace), "--server", "fake")
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if not process_alive(owner_pid):
                break
            time.sleep(0.05)
        else:
            raise SystemExit("Owner remained live five seconds after graceful stop")
        if list(root.glob("**/owners/endpoints/*.json")):
            raise SystemExit("Owner endpoint remained after graceful stop")
        report = {
            "formatVersion": 1,
            "queries": arguments.queries,
            "coldReadinessMs": round(cold_ms, 3),
            "warmP95Ms": round(p95, 3),
            "baseline": baseline,
            "samples": samples,
            "final": final,
            "ownerGeneration": generation,
            "documentChurn": {"retained": open_document_count, "limit": 4},
            "diagnosticChurn": {"retained": diagnostic_snapshot_count, "limit": 4},
        }
        arguments.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(json.dumps(report, sort_keys=True))


if __name__ == "__main__":
    main()
