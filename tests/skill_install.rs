use std::{fs, process::Command};

use serde_json::{Value, json};

#[test]
fn local_install_is_managed_idempotent_and_replace_requires_consent() {
    let workspace = tempfile::tempdir().unwrap();
    let installed = run(workspace.path(), &[]);
    assert!(installed.status.success());
    assert!(installed.stderr.is_empty());
    let installed: Value = serde_json::from_slice(&installed.stdout).unwrap();
    assert_eq!(installed["command"], json!(["skill", "install"]));
    assert_eq!(installed["result"]["scope"], "local");
    assert_eq!(installed["result"]["outcome"], "installed");
    assert_eq!(installed["result"]["skillVersion"], "0.1.0");
    assert_eq!(installed["result"]["previousDigest"], Value::Null);
    let destination = workspace.path().join(".agent/skills/lspc");
    let marker: Value =
        serde_json::from_slice(&fs::read(destination.join(".lspc-managed.json")).unwrap()).unwrap();
    assert_eq!(marker["formatVersion"], 1);
    assert_eq!(marker["manager"], "lspc");
    assert_eq!(marker["digest"], installed["result"]["digest"]);

    let unchanged: Value = serde_json::from_slice(&run(workspace.path(), &[]).stdout).unwrap();
    assert_eq!(unchanged["result"]["outcome"], "unchanged");

    fs::write(destination.join("SKILL.md"), "human edit").unwrap();
    let refused = run(workspace.path(), &[]);
    assert_eq!(refused.status.code(), Some(3));
    let refused: Value = serde_json::from_slice(&refused.stdout).unwrap();
    assert_eq!(refused["error"]["code"], "skill_install_conflict");
    assert_eq!(refused["error"]["data"]["replaceRequired"], true);
    assert_eq!(
        fs::read_to_string(destination.join("SKILL.md")).unwrap(),
        "human edit"
    );

    let replaced: Value =
        serde_json::from_slice(&run(workspace.path(), &["--replace"]).stdout).unwrap();
    assert_eq!(replaced["result"]["outcome"], "replaced");
    assert_ne!(
        fs::read_to_string(destination.join("SKILL.md")).unwrap(),
        "human edit"
    );
}

#[cfg(unix)]
#[test]
fn global_install_uses_the_home_agent_directory() {
    let workspace = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lspc"))
        .args(["skill", "install", "--global"])
        .current_dir(workspace.path())
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["result"]["scope"], "global");
    assert_eq!(
        envelope["result"]["resolvedPath"],
        home.path().join(".agent/skills/lspc").to_str().unwrap()
    );
    assert!(home.path().join(".agent/skills/lspc/SKILL.md").is_file());
}

fn run(workspace: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lspc"));
    command
        .args(["skill", "install"])
        .args(extra)
        .current_dir(workspace);
    command.output().unwrap()
}
