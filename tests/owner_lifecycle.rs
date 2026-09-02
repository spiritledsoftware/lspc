#![cfg(feature = "fake-server")]

use std::{
    fs,
    path::PathBuf,
    process::{Command, Output},
};

use serde_json::{Value, json};
use tempfile::TempDir;

struct Fixture {
    _root: TempDir,
    workspace: PathBuf,
    environment: Vec<(String, PathBuf)>,
}

impl Fixture {
    fn new() -> Self {
        Self::with_server_arguments(&[])
    }

    fn with_server_arguments(arguments: &[&str]) -> Self {
        let root = TempDir::new().unwrap();
        let workspace = root.path().join("workspace");
        fs::create_dir(&workspace).unwrap();

        #[cfg(target_os = "linux")]
        let (config, environment) = {
            let config = root.path().join("config");
            let state = root.path().join("state");
            (
                config.join("lspc/config.toml"),
                vec![
                    ("XDG_CONFIG_HOME".to_owned(), config),
                    ("XDG_STATE_HOME".to_owned(), state),
                    ("HOME".to_owned(), root.path().join("home")),
                ],
            )
        };

        #[cfg(target_os = "macos")]
        let (config, environment) = {
            let home = root.path().join("home");
            (
                home.join("Library/Application Support/lspc/config.toml"),
                vec![("HOME".to_owned(), home)],
            )
        };

        #[cfg(windows)]
        let (config, environment) = {
            let roaming = root.path().join("AppData/Roaming");
            let local = root.path().join("AppData/Local");
            (
                roaming.join("lspc/config.toml"),
                vec![
                    ("APPDATA".to_owned(), roaming),
                    ("LOCALAPPDATA".to_owned(), local),
                    ("USERPROFILE".to_owned(), root.path().join("home")),
                ],
            )
        };

        fs::create_dir_all(config.parent().unwrap()).unwrap();
        let arguments = serde_json::to_string(arguments).unwrap();
        fs::write(
            config,
            format!(
                "version = 1\ndefault_server = \"fake\"\nroutes = [{{ server = \"fake\", language_id = \"rust\", extensions = [\".rs\"] }}]\n[servers.fake]\nexecutable = {:?}\nargs = {}\n",
                env!("CARGO_BIN_EXE_lspc-fake-server"), arguments
            ),
        )
        .unwrap();
        Self {
            _root: root,
            workspace,
            environment,
        }
    }

    fn command(&self, arguments: &[&str]) -> Value {
        let output = self.output(arguments);
        assert!(
            output.status.success(),
            "command failed: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn output(&self, arguments: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lspc"));
        command.args(arguments);
        for (name, value) in &self.environment {
            command.env(name, value);
        }
        command.output().unwrap()
    }
}

#[test]
fn owner_serializes_simultaneous_agent_operations_in_fifo_order() {
    let fixture = Fixture::with_server_arguments(&["--scenario=delayed"]);
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/start",
    ]);

    std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "fixture/first",
            ])
        });
        std::thread::sleep(std::time::Duration::from_millis(10));
        let started = std::time::Instant::now();
        let second = fixture.command(&[
            "raw",
            "--workspace",
            workspace,
            "--server",
            "fake",
            "--method",
            "fixture/second",
        ]);
        assert_eq!(first.join().unwrap()["result"], json!({"fixture": true}));
        assert_eq!(second["result"], json!({"fixture": true}));
        assert!(started.elapsed() >= std::time::Duration::from_millis(60));
    });

    let synchronized = fixture.workspace.join("raw-stale.rs");
    let marker = fixture.workspace.join("request-started");
    fs::write(&synchronized, "old\n").unwrap();
    let raw = std::thread::scope(|scope| {
        let query = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "test/await-file-change",
                "--params-json",
                &json!({"marker": marker}).to_string(),
                "--sync-file",
                synchronized.to_str().unwrap(),
            ])
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(marker.exists());
        fs::write(&synchronized, "new\n").unwrap();
        query.join().unwrap()
    });
    assert_eq!(raw["result"], json!({"fixture": true}));
    assert_eq!(
        raw["context"]["synchronization"]["postResponseChanged"][0]["uri"],
        url::Url::from_file_path(dunce::canonicalize(&synchronized).unwrap())
            .unwrap()
            .to_string()
    );

    fixture.command(&[
        "session",
        "stop",
        "--workspace",
        workspace,
        "--server",
        "fake",
    ]);
}

#[test]
fn queued_operation_deadline_removes_work_before_dispatch() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/start",
    ]);

    let marker = fixture.workspace.join("active-request-started");
    std::thread::scope(|scope| {
        let active = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "test/await-file-change",
                "--params-json",
                &json!({"marker": marker, "sleepMs": 500}).to_string(),
            ])
        });
        let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < marker_deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(marker.exists());
        let expired = fixture.output(&[
            "raw",
            "--workspace",
            workspace,
            "--server",
            "fake",
            "--method",
            "fixture/never-dispatched",
            "--deadline",
            "10ms",
        ]);
        assert_eq!(
            expired.status.code(),
            Some(4),
            "unexpected response: {}",
            String::from_utf8_lossy(&expired.stdout)
        );
        let failure: Value = serde_json::from_slice(&expired.stdout).unwrap();
        assert_eq!(failure["error"]["code"], "queue_deadline_exceeded");
        assert_eq!(failure["error"]["delivery"], "not_sent");
        assert_eq!(active.join().unwrap()["result"], json!({"fixture": true}));
    });

    fixture.command(&[
        "session",
        "stop",
        "--workspace",
        workspace,
        "--server",
        "fake",
    ]);
}

#[test]
fn owner_accepts_status_and_queues_work_during_initialization() {
    let fixture = Fixture::with_server_arguments(&["--scenario=delayed-initialization"]);
    let workspace = fixture.workspace.to_str().unwrap();

    std::thread::scope(|scope| {
        let query = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "fixture/queued-during-initialization",
            ])
        });
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let status = fixture.output(&[
                "session",
                "status",
                "--workspace",
                workspace,
                "--server",
                "fake",
            ]);
            if status.status.success() {
                let status: Value = serde_json::from_slice(&status.stdout).unwrap();
                if status["result"]["state"] == "initializing" {
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "Owner never exposed its initializing state"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        assert_eq!(query.join().unwrap()["result"], json!({"fixture": true}));
    });

    fixture.command(&[
        "session",
        "stop",
        "--workspace",
        workspace,
        "--server",
        "fake",
    ]);
}

#[test]
fn initialization_failure_rejects_queued_work_with_the_same_cause() {
    let fixture = Fixture::with_server_arguments(&["--scenario=crash"]);
    let workspace = fixture.workspace.to_str().unwrap();

    let output = fixture.output(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/never-dispatched",
    ]);
    assert_eq!(
        output.status.code(),
        Some(5),
        "unexpected response: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "initialization_failed");
    assert_eq!(failure["error"]["delivery"], "not_sent");
}

#[test]
fn force_stop_cancels_an_active_query_without_waiting_for_it() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/start",
    ]);

    let marker = fixture.workspace.join("active-request-started");
    std::thread::scope(|scope| {
        let active = scope.spawn(|| {
            fixture.output(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "test/await-file-change",
                "--params-json",
                &json!({"marker": marker, "sleepMs": 5_000}).to_string(),
            ])
        });
        let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < marker_deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(marker.exists());

        let started = std::time::Instant::now();
        let stopped = fixture.command(&[
            "session",
            "stop",
            "--force",
            "--workspace",
            workspace,
            "--server",
            "fake",
        ]);
        assert_eq!(stopped["result"]["outcome"], "force_stopped");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "force stop waited for the active request"
        );

        let active = active.join().unwrap();
        assert!(!active.status.success());
        let failure: Value = serde_json::from_slice(&active.stdout).unwrap();
        assert_eq!(failure["error"]["code"], "request_cancelled");
        assert_eq!(failure["error"]["data"]["source"], "force_stop");
    });
}

#[test]
fn owner_reports_bounded_stderr_after_an_unexpected_server_exit() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let output = fixture.output(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/crash",
    ]);

    assert_eq!(
        output.status.code(),
        Some(5),
        "unexpected response: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "server_exited");
    assert_eq!(failure["error"]["data"]["status"]["code"], 42);
    assert!(
        failure["error"]["data"]["stderrTail"]
            .as_str()
            .is_some_and(|tail| tail.contains("fixture server crashed while handling test/crash"))
    );
}

#[test]
fn owner_bounds_tracks_and_cancels_server_requests() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();

    let bounded = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/server-request-limit",
    ]);
    assert_eq!(bounded["result"], json!({"accepted": 64, "busy": 1}));

    let cancelled = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/cancel-server-request",
    ]);
    assert_eq!(
        cancelled["result"]["callbackResponse"]["error"]["code"],
        -32800
    );

    fixture.command(&[
        "session",
        "stop",
        "--workspace",
        workspace,
        "--server",
        "fake",
    ]);
}

#[test]
fn duplicate_active_server_request_id_terminates_the_owner() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let output = fixture.output(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/duplicate-server-request-id",
    ]);

    assert_eq!(output.status.code(), Some(5));
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "protocol_failed");
    assert!(
        failure["error"]["data"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("reused an active request identifier"))
    );
}

#[test]
fn query_failure_preserves_server_error_partial_results_context_and_trace() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let output = fixture.output(&[
        "workspace-symbols",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--query",
        "error-with-partial",
        "--trace-protocol",
    ]);

    assert_eq!(output.status.code(), Some(5));
    let failure: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(failure["error"]["code"], "server_error");
    assert_eq!(failure["error"]["serverError"]["code"], -32603);
    assert_eq!(failure["method"], "workspace/symbol");
    assert_eq!(failure["partialResult"]["complete"], false);
    assert_eq!(
        failure["partialResult"]["items"][0]["name"],
        "partial-symbol"
    );
    assert_eq!(
        failure["context"]["workspaceUri"],
        url::Url::from_directory_path(dunce::canonicalize(&fixture.workspace).unwrap())
            .unwrap()
            .to_string()
    );
    assert!(failure["trace"]["frames"].as_array().is_some());

    fixture.command(&[
        "session",
        "stop",
        "--workspace",
        workspace,
        "--server",
        "fake",
    ]);
}

#[test]
fn graceful_stop_drains_the_active_query_and_rejects_new_work() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/start",
    ]);

    let marker = fixture.workspace.join("active-request-started");
    std::thread::scope(|scope| {
        let active = scope.spawn(|| {
            fixture.command(&[
                "raw",
                "--workspace",
                workspace,
                "--server",
                "fake",
                "--method",
                "test/await-file-change",
                "--params-json",
                &json!({"marker": marker, "sleepMs": 500}).to_string(),
            ])
        });
        let marker_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !marker.exists() && std::time::Instant::now() < marker_deadline {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        assert!(marker.exists());

        let stop_started = std::time::Instant::now();
        let stop = scope.spawn(|| {
            fixture.command(&[
                "session",
                "stop",
                "--workspace",
                workspace,
                "--server",
                "fake",
            ])
        });
        let state_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let status = fixture.command(&[
                "session",
                "status",
                "--workspace",
                workspace,
                "--server",
                "fake",
            ]);
            if status["result"]["state"] == "draining" {
                break;
            }
            assert!(std::time::Instant::now() < state_deadline);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }

        let rejected = fixture.output(&[
            "raw",
            "--workspace",
            workspace,
            "--server",
            "fake",
            "--method",
            "fixture/rejected",
        ]);
        assert_eq!(rejected.status.code(), Some(4));
        let failure: Value = serde_json::from_slice(&rejected.stdout).unwrap();
        assert_eq!(failure["error"]["code"], "owner_unavailable");
        assert_eq!(failure["error"]["data"]["reason"], "draining");

        assert_eq!(active.join().unwrap()["result"], json!({"fixture": true}));
        let stopped = stop.join().unwrap();
        assert_eq!(stopped["result"]["outcome"], "stopped");
        assert!(stop_started.elapsed() >= std::time::Duration::from_millis(300));
    });
}

#[test]
fn owner_starts_reuses_dispatches_and_stops_without_leaking_output() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace.to_str().unwrap();
    let first = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/first",
        "--params-json",
        "null",
    ]);
    assert_eq!(first["result"], json!({"fixture": true}));
    let generation = first["context"]["ownerGeneration"]
        .as_str()
        .unwrap()
        .to_owned();

    let second = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "fixture/second",
    ]);
    assert_eq!(second["context"]["ownerGeneration"], generation);

    let file = fixture.workspace.join("main.rs");
    fs::write(&file, "fn main() {}\n").unwrap();
    let definition = fixture.command(&[
        "definition",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--file",
        file.to_str().unwrap(),
        "--line",
        "0",
        "--column",
        "3",
    ]);
    assert_eq!(definition["result"], json!([]));
    assert_eq!(definition["context"]["ownerGeneration"], generation);

    let renamed = fixture.workspace.join("rename.rs");
    fs::write(&renamed, "fn old() {}\n").unwrap();
    let rename = fixture.command(&[
        "rename",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--file",
        renamed.to_str().unwrap(),
        "--line",
        "0",
        "--column",
        "4",
        "--new-name",
        "new_name",
    ]);
    assert_eq!(rename["outcome"], "previewed");
    assert_eq!(fs::read_to_string(&renamed).unwrap(), "fn old() {}\n");
    let preview_id = rename["result"]["previewId"].as_str().unwrap();
    let applied = fixture.command(&["apply", preview_id]);
    assert_eq!(applied["outcome"], "applied");
    assert_eq!(applied["result"]["sessionSynchronized"], true);
    assert_eq!(fs::read_to_string(&renamed).unwrap(), "fn new_name() {}\n");
    let receipt = fixture.command(&["receipt", "show", preview_id]);
    assert_eq!(receipt["result"]["outcome"], "applied");
    assert_eq!(receipt["result"]["sessionSynchronized"], true);

    let callback_target = fixture.workspace.join("callback.rs");
    fs::write(&callback_target, "old\n").unwrap();
    let callback_uri = url::Url::from_file_path(dunce::canonicalize(&callback_target).unwrap())
        .unwrap()
        .to_string();
    let callback = fixture.command(&[
        "raw",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--method",
        "test/request-apply-edit",
        "--params-json",
        &json!({"uri": callback_uri}).to_string(),
    ]);
    assert_eq!(callback["result"]["callbackResponse"]["applied"], false);
    assert_eq!(
        callback["result"]["callbackResponse"]["failureReason"],
        "preview_required"
    );
    let callback_preview = callback["result"]["callbackResponse"]["previewId"]
        .as_str()
        .unwrap();
    assert_eq!(
        callback["applyEditLedger"][0]["previewId"],
        callback_preview
    );
    assert_eq!(fs::read_to_string(&callback_target).unwrap(), "old\n");
    assert_eq!(
        fixture.command(&["preview", "show", callback_preview])["result"]["previewId"],
        callback_preview
    );

    let capabilities =
        fixture.command(&["capabilities", "--workspace", workspace, "--server", "fake"]);
    assert_eq!(capabilities["result"]["protocolBaseline"], "3.17");
    assert_eq!(
        capabilities["result"]["providers"]["definition"]["state"],
        "supported"
    );

    let edited = fixture.workspace.join("edited.rs");
    fs::write(&edited, "old\n").unwrap();
    let edited_uri = url::Url::from_file_path(dunce::canonicalize(&edited).unwrap())
        .unwrap()
        .to_string();
    let executed = fixture.command(&[
        "execute-command",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--command",
        "fixture.run",
        "--arguments-json",
        &serde_json::to_string(&vec![edited_uri]).unwrap(),
        "--apply-edits",
    ]);
    assert_eq!(executed["result"]["callbackApplied"], true);
    assert_eq!(executed["applyEditLedger"][0]["applied"], true);
    assert_eq!(executed["applyEditLedger"][0]["outcome"], "applied");
    assert_eq!(fs::read_to_string(edited).unwrap(), "new\n");

    let listed = fixture.command(&["session", "list", "--workspace", workspace]);
    assert_eq!(listed["result"].as_array().unwrap().len(), 1);
    assert_eq!(listed["result"][0]["ownerGeneration"], generation);

    let stopped = fixture.command(&[
        "session",
        "stop",
        "--workspace",
        workspace,
        "--server",
        "fake",
    ]);
    assert_eq!(stopped["result"]["ownerGeneration"], generation);
    assert_eq!(stopped["result"]["outcome"], "stopped");

    let listed = fixture.command(&["session", "list", "--workspace", workspace]);
    assert_eq!(listed["result"], json!([]));
}

#[test]
fn graceful_stop_closes_open_documents_before_shutdown() {
    let root = TempDir::new().unwrap();
    let event_log = root.path().join("events.log");
    let event_argument = format!("--event-log={}", event_log.display());
    let fixture = Fixture::with_server_arguments(&[&event_argument]);
    let workspace = fixture.workspace.to_str().unwrap();
    let document = fixture.workspace.join("open.rs");
    fs::write(&document, "fn main() {}\n").unwrap();

    fixture.command(&[
        "definition",
        "--workspace",
        workspace,
        "--server",
        "fake",
        "--file",
        document.to_str().unwrap(),
        "--line",
        "0",
        "--column",
        "3",
    ]);
    fixture.command(&[
        "session",
        "stop",
        "--workspace",
        workspace,
        "--server",
        "fake",
    ]);

    let events = fs::read_to_string(event_log).unwrap();
    let close = events.find("textDocument/didClose").unwrap();
    let shutdown = events.find("shutdown").unwrap();
    assert!(close < shutdown, "events were out of order:\n{events}");
}
