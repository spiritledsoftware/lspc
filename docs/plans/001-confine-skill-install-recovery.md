# Plan 001: Confine skill-install recovery to validated, authorized installation resources

> **Executor instructions:** Implement only this plan after authorization to execute. Follow the steps and their verification gates. STOP rather than expanding scope when a STOP condition occurs. Update this plan's status and its row in `docs/plans/README.md` when finished; do not execute another plan automatically.
>
> **Drift check (first):** `git diff --stat 5268c6a..HEAD -- src/skill_install.rs tests/skill_install.rs`
> Also run `git status --short`. Compare any changed code with the excerpts below. Reviewed prerequisite changes may be accommodated; unexplained changes to the recovery state machine require a refreshed plan. Do not overwrite another executor's work.

## Status

- **Status:** DONE (Linux verified; native Windows/macOS CI still required before merge)
- **Audit finding:** 1
- **Priority:** P1
- **Effort:** M
- **Risk:** MED
- **Depends on:** none
- **Category:** security
- **Planned at:** commit `5268c6a`, 2026-09-05

## Why this matters

A local skill installation inspects journals inside the selected directory before normal replacement consent. Those files are not trusted evidence of path ownership or authorization: current recovery can rename or recursively delete their unchecked `stage` and `backup` paths. An already-existing `.agent` ancestor symlink also escapes the current parent check. Recovery must fail closed without changing unrelated content while ordinary installation, upgrade, and legitimate interrupted installation remain usable.

## Current state

- `src/skill_install.rs` owns installation, managed markers, staging, journals, and recovery; its inline tests can exercise private helpers.
- `tests/skill_install.rs` checks JSON CLI outcomes and isolates installation in `tempfile::tempdir()`.
- `CONTEXT.md:5–7`: **Agent** is “A software process that invokes shell commands and consumes structured results.” Match the existing JSON `ContractFailure` envelope; do not add diagnostic stdout/stderr text. This installer journal is distinct from the domain's Mutation **Recovery** and its state files.

At `src/skill_install.rs:112–115`, recovery precedes the normal `replace` decision:

```rust
if let Some(journal) = read_journal(parent, scope, &destination)?
    && finish_journal(parent, scope, &journal)?
{
    return Ok(success_result(scope, &journal));
}
```

`read_journal` currently validates only version and destination (`:414–422`):

```rust
if journal.format_version != 1 || journal.destination != destination {
    return Err(install_failure(
        scope,
        destination,
        &io::Error::other("installation journal is incompatible"),
    ));
}
```

`finish_journal`/`restore_journal_backup` (`:425–491`) subsequently rename and delete `journal.stage` and `journal.backup`. `inspect_installation` rejects symlinks but permits an unmanaged ordinary directory, so merely calling it is not deletion authorization. `journal_path` derives the removal filename from the stage basename rather than binding the actual file read.

Production naming (`:214–224`, `:494–503`) is:

- Stage: `.lspctl-stage-` plus 32 lowercase hexadecimal characters.
- Backup: `.lspctl-backup-` plus the **entire stage basename**; retain this existing nested prefix when validating format v1.
- Journal: `.lspctl-journal-` plus the same 32-character identifier and `.json`.

The ancestor bug is the early successful return in `create_safe_parent` (`:505–517`):

```rust
match fs::symlink_metadata(path) {
    Ok(metadata) if !unsafe_metadata(&metadata) && metadata.is_dir() => Ok(()),
```

Existing test convention (`src/skill_install.rs:634–653`):

```rust
let temporary = tempfile::tempdir().unwrap();
let destination = temporary.path().join(".agent/skills/lspctl");
assert_eq!(install_to(&destination, "local", false).unwrap()["outcome"], "installed");
fs::write(destination.join("SKILL.md"), b"modified").unwrap();
let error = install_to(&destination, "local", false).unwrap_err();
assert_eq!(error.code, "skill_install_conflict");
```

`resumes_after_destination_was_moved_to_backup` currently constructs non-production suffixes and an unmanaged backup. Update that fixture deliberately: use production naming and explicitly renew replacement consent when unmanaged material would be destroyed. Do not relax validation just to retain its synthetic names.

## Scope

**Only implementation files allowed:**

- `src/skill_install.rs` — path validation, recovery authorization, inline tests.
- `tests/skill_install.rs` — CLI preservation/error assertions.

**Metadata exception:** this plan and its row in `docs/plans/README.md`.

**Out of scope:** Mutation transaction files, install.sh/install.ps1, bundled skill text, contract schemas, dependency changes, generic filesystem abstraction, public format-version bump. Do not promise immunity to an actively racing process that can replace ancestor directories; this plan closes the demonstrated static-path and recovery-authority defects, not a new cross-platform handle-relative filesystem architecture.

## Commands you will need

Run from the repository root. Use the existing Rust 1.89+ toolchain and locked dependencies; no install step or new dependency is required.

| Purpose | Command | Expected result |
|---|---|---|
| Unit tests | `cargo test --locked --bin lspctl skill_install::tests::` | All selected tests pass |
| CLI tests | `cargo test --locked --test skill_install` | All tests pass |
| Full baseline | `cargo test --locked --all-targets --features fake-server` | All targets pass |
| Lint | `cargo clippy --locked --all-targets --features fake-server -- -D warnings` | Exit 0 |
| Formatting | `cargo fmt --all -- --check` | Exit 0 |
| Contract | `python scripts/release/check_schema.py` | Exit 0 |
| Diff hygiene | `git diff --check` | Exit 0 |

## Git workflow

Leave changes uncommitted. If the operator explicitly authorizes creating a branch, use `advisor/001-confine-skill-install-recovery`. No push or PR. If later asked to commit, use the repository's short imperative style, e.g. `Confine skill installation recovery paths`.

## Steps

### 1. Add preservation-first regression tests

In `src/skill_install.rs::tests`, add these four non-platform-specific tests, using temporary directories and synthetic safe sentinel contents only:

1. `skill_recovery_rejects_unbound_paths` — table-test invalid/aliased stage and backup paths, destination aliases, invalid suffixes, and a journal filename inconsistent with the fields. Assert an existing sentinel outside the validated installation resources is byte-identical and unmoved, and the suspect journal is retained.
2. `skill_recovery_does_not_infer_replace_consent` — a valid-shaped journal cannot replace an unmanaged destination or delete an unmanaged backup without fresh `replace` authorization. Verify preserved destination, backup, and journal.
3. `skill_recovery_preserves_unrecognized_stage` — a same-parent stage with unexpected files, invalid managed marker, or wrong bundle content is preserved, not cleared to make room for a regenerated stage. An unsafe old-version journal must also preserve unknown contents.
4. `skill_recovery_resumes_valid_interruption` — cover before destination move, after move, after stage installation, and pending cleanup using production-generated names. Missing stage may be regenerated only when the expected embedded bundle is known; exercise managed upgrade and explicit replacement paths separately.

Add `skill_recovery_rejects_existing_symlink_ancestor` under `#[cfg(unix)]`: create an existing external directory containing `skills`, point the selected base's `.agent` to it, invoke installation, and assert no lock, stage, journal, or installation was written through it. Keep all targets in test-owned temporary directories. Add Windows reparse-point coverage only if the existing CI privilege model permits creating one; do not silently claim it ran.

**Verify registration:**

```sh
test "$(cargo test --locked --bin lspctl skill_install::tests::skill_recovery_ -- --list | grep -c 'skill_recovery_.*: test$')" -ge 4
```

Expected: exit 0 and all four portable test names listed (plus the Unix test on Unix). Check the names against the list above; zero selected tests cannot satisfy this gate.

**Verify red:** `cargo test --locked --bin lspctl skill_install::tests::skill_recovery_` must fail preservation assertions on the current implementation; the valid-interruption case may already pass. Compilation failures or unrelated setup errors are not a valid red result.

### 2. Bind the paths before taking recovery actions

Validate the actual journal pathname and all deserialized path fields as one unit before `finish_journal`, including calls made from the fresh installation path. Use exact expected same-parent basenames derived from the validated identifier rather than prefix-only or canonical-path containment tests. Require distinct destination/stage/backup/journal resources, absolute paths without traversal components, the supported format, and a syntactically valid digest. Preserve the current v1 naming shape described above. Remove only the exact validated journal file that was read; reject multiple journals as today.

Choose a selected-base boundary explicitly. Resolve the legitimate selected base once (so standard macOS/home aliases above it are not incorrectly forbidden), then inspect **every component below that base**, including already-existing `.agent` and `skills`, with the existing symlink/reparse checks before creating a lock or touching recovery state. Do not follow a suspect ancestor to decide that its target is safe. Adjust private helper arguments if needed to carry the boundary; direct-path tests must supply an unambiguous test base.

**Verify:** run `cargo test --locked --bin lspctl skill_recovery_rejects_unbound_paths` and, on Unix, `cargo test --locked --bin lspctl skill_recovery_rejects_existing_symlink_ancestor`. Each command must run one test and pass. Run `cargo test --locked --test skill_install`; ordinary installation remains green.

### 3. Separate valid journal structure from permission to destroy content

Thread current invocation replacement consent into recovery decisions. Validate existing destination/stage/backup content before any move or recursive deletion; a journal, an `outcome` string, or a merely parseable marker is not sufficient authority.

- A stage may be installed or cleaned up only when its managed marker and actual contents verify against the recognized journal bundle. Unknown existing content must remain untouched. A missing stage may be recreated for the current embedded digest using exclusive creation.
- Apply the ordinary install/managed-upgrade/`--replace` policy to an existing destination even when a journal is present. Do not turn recovery into implicit replacement authorization.
- Before deleting a backup, establish that it matches recognized managed predecessor content, or require fresh explicit replacement consent for unmanaged predecessor content. If the current invocation lacks consent, preserve the backup and report a structured conflict, even if the previous process originally had `--replace`.
- A stale/other-version journal with insufficient evidence must produce a structured failure and preserve its directories. Restore into an absent destination only after confined-path validation; never remove an unknown stage merely to complete rollback.
- Retain the existing lock and durability ordering. Reject contradictory states before making new changes; keep the journal when manual inspection is needed.

Use the existing `conflict`/`install_failure` constructors and existing schema fields; no new error code or success outcome. Update the old interruption fixture to production names and the new explicit-consent policy. Add CLI test `local_recovery_preserves_unmanaged_content_without_replace` to `tests/skill_install.rs`, using its `run` helper and JSON error assertions.

**Verify:** `cargo test --locked --bin lspctl skill_install::tests::` and `cargo test --locked --test skill_install` both exit 0. Test output must show all four portable regressions and the new CLI regression executed.

### 4. Run release-facing gates and record the result

Run every command in the command table. Inspect `git status --short` against the scope and update only this plan's status and index row after all gates pass. Document any native Windows test that remains unexecuted locally; CI must pass on Windows before merge.

**Verify:** `git diff --check` exits 0; `git status --short` contains only allowed implementation files and plan metadata, plus any pre-existing work explicitly recorded before starting.

## Test plan

The four portable `skill_recovery_*` tests cover path binding, consent, stage preservation, and genuine interruption. The Unix test covers the existing-ancestor bypass without a race. The new CLI regression checks an error envelope and preservation, not merely a helper's return value. Keep `installs_upgrades_refuses_and_replaces`, global installation, idempotence, and replacement tests passing. Use table-driven cases inside these tests rather than building a generic recovery test framework.

## Done criteria

- [x] All four portable new tests appear in `cargo test --locked --bin lspctl skill_recovery_ -- --list` and pass; the Unix ancestor regression passes on Unix.
- [x] `local_recovery_preserves_unmanaged_content_without_replace` runs and passes in `--test skill_install`.
- [x] Unrecognized stage/backup/destination contents and invalid journals remain unchanged in preservation assertions.
- [x] Valid interrupted managed installations and explicitly authorized replacements complete under production v1 names.
- [x] Full tests, Clippy, formatting, contract check, and `git diff --check` exit 0.
- [x] Scope is respected; this plan and its index row record completion and platform verification.

## STOP conditions

Stop and report if a journal cannot be distinguished from unowned content without deleting that content, if retaining legitimate format-v1 recovery requires weakening confinement, or if public schema/version changes appear necessary. Stop if the base-path policy would reject standard platform home/temp aliases rather than only suspect components below the selected base. Do not invent a new trust store or cryptographic provenance system. Also stop on unexplained drift, out-of-scope requirements, or the same verification failure after two reasonable correction attempts.

## Execution record — 2026-09-05

Implemented against `5268c6a` on the existing `main` branch. The operator explicitly authorized committing this implementation, superseding the default uncommitted workflow above. All 15 planning documents were untracked before implementation; plans 002–014 and the existing demo plan were left unchanged.

- Journal fields and the actual journal filename are bound to exact production v1 sibling names before recovery side effects. Current invocation consent governs unmanaged destination and backup replacement.
- Existing stages must verify the current embedded bundle, including absence of unexpected directories. Unknown/stale bundle journals fail closed with all resources retained; the unsafe best-effort restore-and-delete path was removed. No public schema or format version changed.
- Only the selected base is canonicalized; every component below it is checked before lock creation. Legitimate old v1 journals using that selected base's original alias spelling are validated and rebound in memory, without resolving journal-controlled paths.
- Red tests demonstrated external sentinel deletion, writes through an existing symlink ancestor, missing replacement consent, and destruction of an unknown stage. The review-discovered legacy-alias regression also failed before its fix, then passed.
- Two-axis review: Standards found two nonblocking cleanup suggestions, both resolved and re-reviewed. Spec found one legacy selected-base alias compatibility defect, fixed and independently rechecked through the CLI. No remaining findings on either axis.

Verification on Linux (Rust commands used `--offline --locked` with installed dependencies):

| Gate | Result |
| --- | --- |
| `cargo test --bin lspctl skill_install::tests::` | 9 passed, including all four portable planned regressions and three Unix regressions |
| `cargo test --test skill_install` | 3 passed, including the new CLI preservation/consent regression |
| `cargo check --all-targets --features fake-server` | Passed throughout implementation and after the final code change |
| `cargo test --all-targets --features fake-server` | 92 passed; full suite run once after review fixes |
| `cargo clippy --all-targets --features fake-server -- -D warnings` | Passed |
| `cargo fmt --all -- --check` | Passed |
| `python scripts/release/check_schema.py` | Passed |
| `python scripts/release/check_stored_state.py` | Passed |
| `git diff --check` | Passed |

Native Windows/macOS runs and Windows reparse-point creation were not performed locally. Native CI remains required before merge. Static-path checks do not claim protection against an actively racing process replacing filesystem ancestors.

## Maintenance notes

Review every `rename`, `remove_dir_all`, and journal removal path, including failure cleanup; path validation must precede side effects, not just the successful branch. Fresh consent for destruction of an unmanaged recovered backup is an intentional safety tightening. Future handle-relative race hardening is separate; do not describe repeated metadata checks as full TOCTOU protection.
