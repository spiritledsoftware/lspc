use std::process::{Command, Output};

use serde_json::{Value, json};

#[test]
fn version_command_and_alias_emit_the_same_machine_envelope() {
    let command_output = run_lspc(&["version"]);
    let alias_output = run_lspc(&["--version"]);

    assert!(command_output.status.success());
    assert!(command_output.stderr.is_empty());
    assert_eq!(command_output.stdout, alias_output.stdout);
    assert_eq!(command_output.stdout.last(), Some(&b'\n'));

    let envelope: Value = serde_json::from_slice(&command_output.stdout).unwrap();
    assert_eq!(envelope["schemaVersion"], 1);
    assert_eq!(envelope["ok"], true);
    assert_eq!(envelope["command"], json!(["version"]));
    assert_eq!(envelope["result"]["name"], "lspc");
    assert_eq!(envelope["result"]["version"], "1.0.0");
    assert_eq!(envelope["result"]["contractVersion"], 1);
    assert_eq!(envelope["result"]["configVersion"], 1);
    assert_eq!(envelope["result"]["capabilityProfileVersion"], 1);
    assert_eq!(envelope["result"]["ownerProtocolVersion"], 1);
    assert!(envelope["result"]["target"].as_str().unwrap().contains('-'));

    let commit = envelope["result"]["commit"].as_str().unwrap();
    assert!((7..=64).contains(&commit.len()));
    assert!(commit.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

#[test]
fn invalid_cli_emits_a_structured_failure_without_stderr() {
    let output = run_lspc(&["-V"]);

    assert_eq!(output.status.code(), Some(2));
    assert!(output.stderr.is_empty());
    assert_eq!(output.stdout.last(), Some(&b'\n'));

    let envelope: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(envelope["schemaVersion"], 1);
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["command"], json!([]));
    assert_eq!(envelope["error"]["category"], "input");
    assert_eq!(envelope["error"]["code"], "invalid_arguments");
    assert_eq!(envelope["error"]["stage"], "parse_cli");
    assert_eq!(envelope["error"]["delivery"], "not_applicable");
    assert_eq!(envelope["error"]["retry"], "never");
    assert_eq!(
        envelope["error"]["data"]["problems"][0]["code"],
        "invalid_value"
    );
}

fn run_lspc(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lspc"))
        .args(arguments)
        .output()
        .unwrap()
}
