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
        fs::write(
            config,
            format!(
                "version = 1\ndefault_server = \"fake\"\nroutes = [{{ server = \"fake\", language_id = \"rust\", extensions = [\".rs\"] }}]\n[servers.fake]\nexecutable = {:?}\n",
                env!("CARGO_BIN_EXE_lspc-fake-server")
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

    let capabilities =
        fixture.command(&["capabilities", "--workspace", workspace, "--server", "fake"]);
    assert_eq!(capabilities["result"]["protocolBaseline"], "3.17");
    assert_eq!(
        capabilities["result"]["providers"]["definition"]["state"],
        "supported"
    );

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
