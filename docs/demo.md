# README Agent rename demo

This is the evidence record for the [README video](../assets/demo/lspctl-agent-rename.mp4). It compares baseline Pi with Pi using the released `lspctl` CLI and bundled skill on one semantic rename. It is a demonstration, not a benchmark or a general productivity claim.

## Comparison contract

| Input | Value |
| --- | --- |
| Codebase | [`tokio-rs/tokio`](https://github.com/tokio-rs/tokio) tag `tokio-1.53.1` |
| Tokio commit | `75fef53d0a8590c2d1dbb63672aa7b7d1ef51155` |
| Harness | Pi `0.84.4`, regular TUI mode |
| Model | `openai-codex/gpt-5.6-sol`, high thinking |
| Treatment | `lspctl` `0.1.1` and its bundled skill, built from `dfb7caa0bec65a81d8e2c5c652e0b3a95fb1ffaf` |
| Baseline | Pi without skills or an `lspctl` executable on `PATH` |
| Common tools | `read,bash,edit,write,grep,find,ls` |
| Validation | `cargo test -p tokio --features full,test-util --lib sync::mpsc` |

`lspctl version` reported target `x86_64-unknown-linux-gnu`, commit `dfb7caa0bec65a81d8e2c5c652e0b3a95fb1ffaf`, contract version 1, configuration version 1, capability profile version 1, and Owner protocol version 1. The language server was `rust-analyzer 1.98.0 (88d9e12 2026-08-18)`.

Both Agents used this system prompt:

> You are an expert coding assistant. Use the available tools to complete the user's task in the current repository. Make only requested changes, run the requested validation, and report changed files and the validation result concisely.

Both Agents received this exact task prompt once:

> Rename the exact method `tokio::sync::mpsc::chan::Tx::send`, declared in
> `tokio/src/sync/mpsc/chan.rs`, to `send_value` everywhere it is semantically
> referenced. Preserve behavior and avoid unrelated changes. Run
> `cargo test -p tokio --features full,test-util --lib sync::mpsc`, then report the changed files and
> validation result.

## Isolation and effective startup

The comparison used detached worktrees at the pinned Tokio commit, separate empty Pi session directories, one temporary auth-only `PI_CODING_AGENT_DIR`, and an empty project context. Both runs used:

```sh
pi \
  --model openai-codex/gpt-5.6-sol \
  --thinking high \
  --tools read,bash,edit,write,grep,find,ls \
  --no-extensions \
  --no-skills \
  --no-prompt-templates \
  --no-themes \
  --no-context-files \
  --no-approve \
  --tui-mode regular \
  --session-dir "$SESSION_DIR" \
  --system-prompt "$SYSTEM_PROMPT"
```

The treatment added only:

```sh
PATH="$LSPCTL_RELEASE_DIR:$PATH"
--skill "$DEMO_ROOT/treatment-skill-root/.agent/skills/lspctl/SKILL.md"
```

The baseline could not resolve `lspctl`; the treatment resolved the released binary. Ambient extensions, skills, prompt templates, themes, and context files were disabled. The native Linux lspctl configuration resolved to `~/.config/lspctl/config.toml`; it did not exist before the run, was created temporarily with the Rust route below, and was removed afterward.

```toml
version = 1
default_server = "rust"
routes = [{ server = "rust", language_id = "rust", extensions = [".rs"] }]

[servers.rust]
executable = "rust-analyzer"
request_timeout = "2m"
```

## Prepared state and prewarming

The validation command was compiled in each worktree before the Agents ran, using the same external Cargo target directory. Immediately before the valid treatment recording, a throwaway Pi process in the same Herdr pane and environment ran a semantic references Query, polled the Owner until `progress` was empty, and confirmed five semantic locations.

The warm Owner generation was `gen_461feafa99f627532f5543b65405f453`, with Session identity `sid_2657fe415289fd459314a2b8bd45e6cfbe2c930f3894ce44ee26a94da118308d`. The treatment's first lspctl Query was `rename`; the retained post-run Owner had the same generation, and the Application Receipt had the same Session identity. Rust-analyzer was therefore indexed before recording rather than during the treatment.

## Ground truth and scoring

A disposable reference Workspace used the same released CLI to request the rename, inspect the Preview, authorize its Application, inspect the Receipt, and run the fixed validation. The normalized reference was saved with:

```sh
git diff --no-ext-diff --binary
```

It contained three files and five text edits. The exact semantic locations below use zero-based LSP line and UTF-8 character ranges:

| File | Ranges |
| --- | --- |
| `tokio/src/sync/mpsc/chan.rs` | `179:18–22` |
| `tokio/src/sync/mpsc/bounded.rs` | `931:18–22`, `1723:18–22`, `1848:13–17` |
| `tokio/src/sync/mpsc/unbounded.rs` | `551:18–22` |

Each Agent's final normalized diff was compared byte-for-byte with that reference. A “discovery tool call” is a Pi tool call before the first filesystem edit in the baseline or before the Application in the treatment. A “non-target source file opened” is an explicit Pi `read` of a Rust source file other than the declared `chan.rs`; textual search results and the treatment Preview are not counted as opened files.

## Observed result

| Observation | Baseline Pi | Pi + lspctl |
| --- | ---: | ---: |
| Reference diff match | Yes | Yes |
| Files changed / text edits | 3 / 5 | 3 / 5 |
| Fixed validation | Passed (1 test) | Passed (1 test) |
| Discovery tool calls | 12 | 4 |
| Non-target source files opened | 0 | 0 |
| Total tool calls | 20 | 8 |
| Raw wall time | 69 s | 54 s |

The treatment used a semantic rename Query to produce a five-edit Preview, inspected that Preview before the Application, and retained the successful Receipt ID and filesystem state. These observations support only the narrow claim shown in the README: semantic navigation plus an inspectable Preview can reduce blind source inspection and guard the resulting Application on this task.

## Pilot and restart history

The task prompt was not changed.

- The first pilot did not reuse the warm Owner because prewarming did not reproduce Pi's effective shell environment.
- The second pilot still selected a different Session identity, encountered language-server synchronization failures, and exceeded the baseline tool-call count.
- The third pilot used a prewarm Query from Pi's own shell environment. Both diffs matched ground truth, both tests passed, the treatment started with `rename`, inspected and applied its Preview, retained the Receipt fields, reused the warm Owner, and used 10 tool calls versus the baseline's 15. It passed the gate.
- The first baseline capture was discarded because computer-use's Linux trajectory recording reported `video.present: false`; no Agent mistake caused the restart.
- The first treatment capture was discarded because its pane had not inherited the shared Cargo target directory, violating the prepared-dependency contract. The environment was corrected, the treatment Workspace was prewarmed again, and the task was rerun.

Computer-use was used to launch and maximize Ghostty, collapse the Herdr sidebar, and verify the labelled panes and visible screen. Its available FFmpeg build lacked `x11grab`, so the valid raw screen captures used `xwd` frames encoded by FFmpeg instead of the trajectory recorder's video option. The final edit remains silent H.264/yuv420p and preserves the full prompt-to-validation interaction; the displayed wall times come from Herdr prompt waits, not encoded video duration.

## Limitations

- This is one pair of runs on one internal Tokio rename, not a benchmark.
- The treatment includes both the released CLI and bundled skill; the recording does not isolate their individual effects.
- Dependencies were prepared for both runs, and rust-analyzer was additionally pre-indexed for the treatment as disclosed above and in the video.
- Model behavior is nondeterministic. Another run may choose different searches or shell grouping.
- Both Agents produced the correct reference diff here; the result does not establish a general correctness or speed advantage.
- “Tool calls” counts Pi tool invocations, not individual shell commands grouped inside one `bash` call.

## Sanitized transcripts

The transcripts include the user prompt, tool calls, and final responses. Tool outputs, credentials, local authentication details, hidden model reasoning, and unrelated environment data are omitted. `$DEMO_ROOT` replaces the local temporary evidence directory.

<details>
<summary>Baseline Pi transcript</summary>

```text
User: Rename the exact method `tokio::sync::mpsc::chan::Tx::send`, declared in `tokio/src/sync/mpsc/chan.rs`, to `send_value` everywhere it is semantically referenced. Preserve behavior and avoid unrelated changes. Run `cargo test -p tokio --features full,test-util --lib sync::mpsc`, then report the changed files and validation result.

Tool calls:
1. grep {"context": 4, "literal": true, "path": "tokio/src/sync/mpsc/chan.rs", "pattern": "fn send"}
2. grep {"context": 1, "limit": 100, "literal": true, "path": "tokio", "pattern": ".send("}
3. grep {"context": 2, "limit": 100, "literal": true, "path": "tokio/src/sync/mpsc", "pattern": "Tx<"}
4. grep {"context": 4, "limit": 100, "literal": true, "path": "tokio/src/sync/mpsc", "pattern": "chan.send("}
5. grep {"context": 2, "limit": 100, "literal": true, "path": "tokio/src/sync/mpsc", "pattern": "self.inner.send("}
6. grep {"context": 3, "limit": 100, "literal": true, "path": "tokio/src/sync/mpsc", "pattern": ".send(value"}
7. bash {"command": "git status --short && rg -n '\\.send\\(' tokio/src/sync/mpsc"}
8. read {"limit": 80, "offset": 130, "path": "tokio/src/sync/mpsc/chan.rs"}
9. grep {"context": 1, "limit": 200, "literal": true, "path": ".", "pattern": ".chan.send("}
10. grep {"context": 1, "limit": 200, "literal": true, "path": ".", "pattern": "chan.send(value)"}
11. grep {"context": 1, "limit": 200, "literal": true, "path": ".", "pattern": "chan::Tx"}
12. grep {"context": 1, "limit": 200, "literal": true, "path": ".", "pattern": "Tx::send"}
13. edit {"edits": [{"newText": "    /// Send a message and notify the receiver.\n    pub(crate) fn send_value(&self, value: T) {\n        self.inner.send(value);\n    }", "oldText": "    /// Send a message and notify the receiver.\n    pub(crate) fn send(&self, value: T) {\n        self.inner.send(value);\n    }"}], "path": "tokio/src/sync/mpsc/chan.rs"}
14. edit {"edits": [{"newText": "        // Send the message\n        self.chan.send_value(message);\n        Ok(())", "oldText": "        // Send the message\n        self.chan.send(message);\n        Ok(())"}, {"newText": "        self.chan.send_value(value);\n\n        // Avoid the drop logic", "oldText": "        self.chan.send(value);\n\n        // Avoid the drop logic"}, {"newText": "        chan.send_value(value);\n\n        Sender { chan }", "oldText": "        chan.send(value);\n\n        Sender { chan }"}], "path": "tokio/src/sync/mpsc/bounded.rs"}
15. edit {"edits": [{"newText": "        self.chan.send_value(message);\n        Ok(())", "oldText": "        self.chan.send(message);\n        Ok(())"}], "path": "tokio/src/sync/mpsc/unbounded.rs"}
16. bash {"command": "git diff --check && git diff -- tokio/src/sync/mpsc/chan.rs tokio/src/sync/mpsc/bounded.rs tokio/src/sync/mpsc/unbounded.rs"}
17. grep {"context": 1, "limit": 100, "literal": true, "path": ".", "pattern": "send_value"}
18. grep {"context": 1, "limit": 100, "literal": true, "path": "tokio/src/sync/mpsc", "pattern": "chan.send("}
19. bash {"command": "cargo test -p tokio --features full,test-util --lib sync::mpsc", "timeout": 1200}
20. bash {"command": "git status --short"}

Assistant: Renamed `tokio::sync::mpsc::chan::Tx::send` to `send_value` and updated all semantic references.

Changed files:
- `tokio/src/sync/mpsc/chan.rs`
- `tokio/src/sync/mpsc/bounded.rs`
- `tokio/src/sync/mpsc/unbounded.rs`

Validation:
- `cargo test -p tokio --features full,test-util --lib sync::mpsc`
- Passed: 1 test, 0 failed.
```

</details>

<details>
<summary>Pi + lspctl transcript</summary>

```text
User: Rename the exact method `tokio::sync::mpsc::chan::Tx::send`, declared in `tokio/src/sync/mpsc/chan.rs`, to `send_value` everywhere it is semantically referenced. Preserve behavior and avoid unrelated changes. Run `cargo test -p tokio --features full,test-util --lib sync::mpsc`, then report the changed files and validation result.

Tool calls:
1. read {"path": "$DEMO_ROOT/treatment-skill-root/.agent/skills/lspctl/SKILL.md"}
2. bash {"command": "git status --short && grep -n \"fn send\" tokio/src/sync/mpsc/chan.rs", "timeout": 30}
3. read {"limit": 80, "offset": 160, "path": "tokio/src/sync/mpsc/chan.rs"}
4. bash {"command": "set -o pipefail\nrename_json=$(lspctl rename --workspace . --server rust --file tokio/src/sync/mpsc/chan.rs --line 179 --column 18 --new-name send_value) || { status=$?; printf '%s\\n' \"$rename_json\"; exit $status; }\npreview_id=$(printf '%s\\n' \"$rename_json\" | jq -er 'select(.ok and .outcome == \"previewed\") | .result.previewId') || { printf '%s\\n' \"$rename_json\"; exit 1; }\nprintf '%s\\n' \"$rename_json\" | jq '{outcome, previewId: .result.previewId, summary: .result.summary}'\nprintf '%s\\n' \"PREVIEW_ID=$preview_id\"\nlspctl preview show \"$preview_id\" | jq '{ok, result: {previewId: .result.previewId, summary: .result.summary, conflicts: .result.conflicts, staleReasons: .result.staleReasons, operations: [.result.plan.operations[] | {kind, path, oldPath, newPath, edits}], diff: .result.diff}}'", "timeout": 120}
5. bash {"command": "lspctl apply prv_6fd7e71aa29bec3c6aa01caccb729513 --workspace . --server rust | jq '{outcome, result: {receiptId: .result.receiptId, filesystemState: .result.filesystemState, sessionSynchronized: .result.sessionSynchronized}}'", "timeout": 120}
6. bash {"command": "cargo test -p tokio --features full,test-util --lib sync::mpsc", "timeout": 1200}
7. bash {"command": "git status --short && git diff --check && git diff --stat", "timeout": 30}
8. bash {"command": "git diff -- tokio/src/sync/mpsc/chan.rs tokio/src/sync/mpsc/bounded.rs tokio/src/sync/mpsc/unbounded.rs", "timeout": 30}

Assistant: Renamed `Tx::send` to `send_value` across semantic references.

Changed files:
- `tokio/src/sync/mpsc/chan.rs`
- `tokio/src/sync/mpsc/bounded.rs`
- `tokio/src/sync/mpsc/unbounded.rs`

Validation:
- `cargo test -p tokio --features full,test-util --lib sync::mpsc`
- Passed: 1 test, 0 failed.
```

</details>
