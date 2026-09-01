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
        url::Url::from_file_path(&synchronized).unwrap().to_string()
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
    let callback_uri = url::Url::from_file_path(&callback_target)
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
    let edited_uri = url::Url::from_file_path(&edited).unwrap().to_string();
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
