#![cfg(feature = "fake-server")]

use std::{
    io::{BufRead, BufReader, Read, Write},
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

fn fixture(scenario: &str) -> (Child, ChildStdin, BufReader<ChildStdout>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_lspctl-fake-server"))
        .arg(format!("--scenario={scenario}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let input = child.stdin.take().unwrap();
    let output = BufReader::new(child.stdout.take().unwrap());
    (child, input, output)
}

fn send(output: &mut impl Write, message: Value, chunks: usize) {
    let body = serde_json::to_vec(&message).unwrap();
    let mut frame = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    frame.extend(body);
    for chunk in frame.chunks(chunks) {
        output.write_all(chunk).unwrap();
        output.flush().unwrap();
    }
}

fn receive(input: &mut impl BufRead) -> Value {
    let mut length = None;
    loop {
        let mut line = String::new();
        input.read_line(&mut line).unwrap();
        if line == "\r\n" {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = Some(value.trim().parse::<usize>().unwrap());
        }
    }
    let mut body = vec![0; length.unwrap()];
    input.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn fixture_fragments_frames_and_emits_lsp_callbacks() {
    let (mut child, mut input, mut output) = fixture("fragmented");
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}),
        2,
    );
    let responses = (0..5).map(|_| receive(&mut output)).collect::<Vec<_>>();
    for method in [
        "window/logMessage",
        "$/progress",
        "textDocument/publishDiagnostics",
        "client/registerCapability",
    ] {
        assert!(
            responses
                .iter()
                .any(|response| response["method"] == method)
        );
    }
    assert!(
        responses
            .iter()
            .any(|response| response["id"] == 1 && response["result"]["capabilities"] == json!({}))
    );
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn fixture_delays_and_reorders_ids() {
    let (mut child, mut input, mut output) = fixture("out-of-order");
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":"slow","method":"test/slow"}),
        100,
    );
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":2,"method":"test/fast"}),
        100,
    );
    assert_eq!(receive(&mut output)["id"], 2);
    assert_eq!(receive(&mut output)["id"], "slow");
    child.kill().unwrap();
    child.wait().unwrap();
    let (mut child, mut input, mut output) = fixture("delayed");
    send(
        &mut input,
        json!({"jsonrpc":"2.0","id":3,"method":"test/fast"}),
        100,
    );
    let start = Instant::now();
    assert_eq!(receive(&mut output)["id"], 3);
    assert!(start.elapsed() >= Duration::from_millis(30));
    child.kill().unwrap();
    child.wait().unwrap();
}

#[test]
fn fixture_exposes_invalid_frames_crash_and_hang() {
    for scenario in [
        "malformed-header",
        "malformed-json",
        "conflicting-length",
        "oversized-frame",
    ] {
        let (mut child, _, mut output) = fixture(scenario);
        let mut bytes = Vec::new();
        output.read_to_end(&mut bytes).unwrap();
        assert!(!bytes.is_empty());
        assert!(child.wait().unwrap().success());
    }
    let (mut child, _, _) = fixture("crash");
    assert_eq!(child.wait().unwrap().code(), Some(42));
    let (mut child, _, _) = fixture("hang");
    std::thread::sleep(Duration::from_millis(20));
    assert!(child.try_wait().unwrap().is_none());
    child.kill().unwrap();
    child.wait().unwrap();
}
