# README agent demo plan

## Outcome

Add a reproducible README demo comparing baseline Pi with Pi using the released
`lspctl` CLI and bundled skill. Both Agents rename the same internal Tokio
method. The demo makes a narrow claim: semantic navigation plus an inspectable
Preview can reduce blind source inspection and guard the resulting Application
on this task. It is a demonstration, not a benchmark or general productivity
claim.

Produce only these repository files:

- `README.md` — a short “See lspctl in action” section after the introductory
  capability list.
- `assets/demo/lspctl-agent-rename.mp4` — the silent final video, at most 10 MB.
- `assets/demo/lspctl-agent-rename.webp` — an accessible poster linked to the
  committed MP4.
- `docs/demo.md` — methodology, exact inputs, observed results, and collapsed
  transcripts.

Keep setup scripts, Tokio worktrees, Pi sessions, and raw recordings outside
the repository.

## Fixed comparison contract

| Input | Value |
| --- | --- |
| Codebase | `tokio-rs/tokio` tag `tokio-1.53.1` |
| Tokio commit | `75fef53d0a8590c2d1dbb63672aa7b7d1ef51155` |
| Harness | Pi `0.84.4`, regular TUI mode |
| Model | `openai-codex/gpt-5.6-sol`, high thinking |
| Treatment | Candidate `lspctl` and bundled skill from the same pinned commit; pin the commit containing the Agent-workflow fixes before recording |
| Baseline | Pi without skills or an `lspctl` executable on `PATH` |
| Common tools | `read,bash,edit,write,grep,find,ls` |
| Validation | `cargo test -p tokio --features full,test-util --lib sync::mpsc` |

Use the same Pi flags, system prompt, task prompt, environment policy, prepared
dependency state, and separate empty session directory for each Agent. Disable
ambient extensions, skills, prompt templates, themes, and context files. The
only treatment differences are the bundled `lspctl` skill and access to the
released binary; describe the comparison with that wording everywhere.

Send both Agents this exact prompt:

> Rename the exact method `tokio::sync::mpsc::chan::Tx::send`, declared in
> `tokio/src/sync/mpsc/chan.rs`, to `send_value` everywhere it is semantically
> referenced. Preserve behavior and avoid unrelated changes. Run
> `cargo test -p tokio --features full,test-util --lib sync::mpsc`, then report the changed files and
> validation result.

## Execution

### 1. Establish the reference result

1. After the Agent-workflow fixes are reviewed and committed, build that exact
   commit with `cargo build --release --locked` and save the version and commit
   reported by `target/release/lspctl version`.
2. Clone Tokio once into a temporary directory at the pinned commit. Create a
   disposable reference worktree and separate rehearsal and recording
   worktrees for both Agents.
3. In the reference worktree, perform the semantic rename, inspect the Preview,
   authorize its Application, inspect the Receipt, run the fixed validation,
   and save the normalized Git diff as ground truth.
4. Review every changed range against the `Tx::send` declaration and semantic
   references. Record the exact changed-file and changed-range set.

**Complete when:** the reference diff contains only the semantic rename, the
fixed validation passes, and the expected files and ranges are recorded before
either Agent runs.

### 2. Prepare isolated, comparable Agents

1. Protect any existing native user configuration, then create the minimal
   temporary Rust server declaration at
   `~/Library/Application Support/lspctl/config.toml`. Arrange cleanup with a
   shell trap so interruption restores the original state.
2. Prebuild the fixed validation command in every rehearsal and recording
   worktree. Pre-index the treatment Workspace with `rust-analyzer` under the
   same effective server environment Pi will expose. Save its Owner generation
   and require the Agent's first Query to reuse it. Disclose this warm state in
   the video and `docs/demo.md`.
3. Give each Agent an empty Pi session directory. Start both with the fixed
   model, thinking level, common tools, `--no-extensions`, `--no-skills`,
   `--no-prompt-templates`, `--no-themes`, `--no-context-files`, the same
   approval policy, and `--tui-mode regular`. Load the bundled skill explicitly
   only for treatment.
4. Keep the baseline `PATH` free of `lspctl`; prepend the released binary only
   for treatment. Preserve all other environment variables.

**Complete when:** both Agents start cleanly in equivalent worktrees, the
treatment Owner reports no indexing progress, the baseline cannot resolve
`lspctl`, and the exact effective Pi flags and environment differences are
saved for `docs/demo.md`.

### 3. Run the pilot gate

1. Run one unrecorded pair with the frozen comparison contract and exact task
   prompt.
2. Compare each resulting diff with ground truth and verify the fixed test.
3. From the Pi sessions, count discovery tool calls and non-target source files
   opened. Treat elapsed time and total tool calls as secondary observations.
4. Confirm the treatment visibly uses semantic Queries, inspects its Preview,
   authorizes the Application, and verifies the Receipt.

Proceed only if both results match ground truth and the treatment uses the
prewarmed Owner, starts with `rename` rather than schema or navigation
preflights, inspects the Preview, applies it, and uses fewer total tool calls
than the baseline. If the task does not expose that difference, revise and
re-freeze the task before recording. A material task change requires user
review.

**Complete when:** the pilot passes the gate with recorded evidence, or work is
paused with the failed criterion and proposed task revision.

### 4. Record the first valid pair

1. Create a new Herdr workspace with two side-by-side panes labelled
   **Baseline Pi** and **Pi + lspctl**. Use Herdr CLI commands for pane creation,
   Agent startup, prompting, waits, and transcript reads.
2. Use computer-use to maximize Ghostty, hide the Herdr sidebar, set readable
   terminal text, verify no credentials or unrelated windows are visible, and
   start and stop each screen recording.
3. Run the Agents sequentially in fresh recording worktrees, baseline first.
   Send the exact prompt once to each and preserve the raw elapsed time. Record
   each pane separately for later alignment.
4. Restart both recorded runs only for a technical failure such as an API,
   capture, or language-server error. Record the reason. Agent mistakes and a
   weak result are evidence, not technical failures.

**Complete when:** both raw recordings show the full task from prompt through
validation, both Pi sessions are retained, and any restart has a documented
technical reason.

### 5. Score and edit the evidence

1. Compare each final diff byte-for-byte with the normalized ground truth and
   record the validation outcome, discovery calls, non-target source reads,
   total tool calls, and raw elapsed time.
2. Build a silent 60–90 second side-by-side edit. Preserve Herdr pane labels and
   add only these callouts where supported by observed evidence:
   **semantic references**, **preview before apply**, and **verified receipt**.
3. End with the factual results, not a winner banner. State that both runs use
   the same prompt, model, Tokio commit, and prepared dependency state, and
   that rust-analyzer was indexed before recording.
4. Encode H.264/yuv420p with no audio and fast-start metadata. Favor shortening
   dead time over shrinking terminal text. Export at a readable resolution and
   keep the final MP4 at or below 10 MB.
5. Export a small WebP poster showing both labelled panes and the task. Keep the
   source recordings outside the repository until the user approves the edit.

**Complete when:** `ffprobe` confirms the intended codec, duration, dimensions,
and absence of audio; the MP4 meets the size cap; terminal text is readable at
README width; and every displayed metric matches the retained evidence.

### 6. Add the README and evidence document

1. Add “See lspctl in action” after the README capability list. Link the poster
   to `assets/demo/lspctl-agent-rename.mp4`, give the poster descriptive alt
   text, summarize the controlled setup in one sentence, and link
   `docs/demo.md`.
2. In `docs/demo.md`, record the fixed comparison contract, exact prompt,
   prewarming policy, restart history, ground-truth scoring method, observed
   result table, and limitations. Use the domain terms Query, Preview,
   Application, Receipt, Owner, and Workspace as defined in `CONTEXT.md`.
3. Add sanitized, collapsed transcripts containing prompts, tool calls, and
   final responses. Omit credentials, local authentication details, hidden
   model reasoning, and unrelated environment data.
4. Keep claims scoped to this recorded task. State explicitly that the
   treatment includes both the released CLI and bundled skill and that one pair
   is not a benchmark.

**Complete when:** the README works without external media hosting, the poster
links to the committed MP4, `docs/demo.md` lets a reader reproduce and audit
the comparison, and every claim is supported by recorded evidence.

### 7. Verify and pause for review

1. Run `git diff --check` and verify every new relative link resolves.
2. Re-run `ffprobe` and the asset-size check from the committed paths.
3. Inspect the README rendering locally and at narrow width for legibility and
   keyboard-accessible navigation to the video and transcript.
4. Restore or remove the temporary lspctl user configuration, stop only Owners
   and Herdr resources created for the demo, remove disposable worktrees and Pi
   sessions after extracting the transcripts, and confirm the original user
   state is restored.
5. Leave the repository changes uncommitted. Retain raw recordings outside the
   repository until the user approves the final edit.

**Complete when:** verification passes, `git status` lists only this plan and the
four planned repository outputs, temporary configuration and processes are
gone, raw video locations are reported, and execution pauses for user review.
