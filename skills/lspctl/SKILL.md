---
name: lspctl
description: Use the lspctl CLI for language-server Queries and Mutations. Invoke for definitions, references, symbols, diagnostics, rename, formatting, code actions, raw LSP methods, server configuration, project Trust, protocol tracing, or Recovery.
---

# lspctl

Use `lspctl` for semantic code navigation and language-server-proposed changes.

## Run routine Queries directly

Keep one canonical Workspace root and named server across related calls. Named
source positions are zero-based Unicode-scalar coordinates. Point at the first
character of the identifier, rather than indentation or punctuation.

```sh
lspctl definition --workspace . --server rust \
  --file src/lib.rs --line 12 --column 8
lspctl references --workspace . --server rust \
  --file src/lib.rs --line 12 --column 8 --include-declaration true
```

Use a named Query instead of textual search when the task asks about symbol
identity. Read [QUERYING.md](QUERYING.md) for paging, diagnostics, raw methods,
or protocol tracing.

If syntax is uncertain, run `lspctl help <command path>`. Use `lspctl schema`
only when implementing an integration that must validate the exact JSON
contract.

## Rename through one guarded workflow

An explicit rename starts with `rename`; the resulting Preview is the semantic
reference set. `prepare-rename`, `definition`, and `references` are separate
Queries for tasks that explicitly need those results or for diagnosing a
failed rename.

Run the proposal, inspection, and Application in order:

```sh
rename_json=$(lspctl rename --workspace . --server rust \
  --file src/lib.rs --line 12 --column 8 --new-name replacement) || exit
preview_id=$(printf '%s\n' "$rename_json" | jq -er \
  'select(.ok and .outcome == "previewed") | .result.previewId') || exit

printf '%s\n' "$rename_json" |
  jq '{outcome, previewId: .result.previewId, summary: .result.summary}'

lspctl preview show "$preview_id" |
  jq '{ok, result: {
    previewId: .result.previewId,
    summary: .result.summary,
    conflicts: .result.conflicts,
    staleReasons: .result.staleReasons,
    operations: [.result.plan.operations[] |
      {kind, path, oldPath, newPath, edits}],
    diff: .result.diff
  }}'

lspctl apply "$preview_id" --workspace . --server rust |
  jq '{outcome, result: {
    receiptId: .result.receiptId,
    filesystemState: .result.filesystemState,
    sessionSynchronized: .result.sessionSynchronized
  }}'
```

Inspect the Preview before Application. Apply without another question when it
matches the requested rename and has no conflicts or stale reasons. Pause on
unexpected resources, unexplained resource operations, conflicts, or
insufficient evidence. A Preview-only request stops after inspection.

Read [MUTATIONS.md](MUTATIONS.md) for formatting, code actions,
`workspace/applyEdit`, stale Previews, Receipts, or Recovery.

## Select and authorize the server

Follow [CONFIGURATION.md](CONFIGURATION.md) when the human asks to add,
configure, route, or verify a language server. Pass explicit server launch
fields only when the task already establishes them. Invocation-scoped launch
fields create no persistent Trust grant.

Project launch fields require a declaration-bound Trust grant. User configuration and explicit invocation fields do not. Use `lspctl trust status` to inspect the current state before changing it. A grant authorizes the current declaration digest, `trust revoke` removes either a grant or a Denial, and a Denial keeps the declaration blocked until explicitly replaced.

When an error has `code: "project_trust_required"`:

1. Read the Workspace URI, server, declaration digest, and `requiredCommand` from `error.data`.
2. Show those exact details to the human.
3. Run the supplied Trust command only after the human authorizes that Workspace, server, and digest.

Do not broaden a server grant to `trust grant --all`. A durable Denial may be replaced only when the human explicitly authorizes the schema-declared replacement flag.

Run `lspctl capabilities --workspace WORKSPACE --server SERVER` when server
support matters. Use normalized provider states of `supported`, `unsupported`,
and `invalid`.

## Process every envelope

Parse stdout as one JSON object. Use stderr only when `lspctl` could not serialize an envelope.

On success, read `result` together with `context`, synchronization metadata, diagnostics metadata, paging, warnings, traces, and the `applyEditLedger` when present. Do not discard metadata that qualifies the result.

On failure, branch on stable fields rather than `error.message`:

| Condition | Action |
| --- | --- |
| `retry: "safe"` | The same invocation may be repeated. |
| `retry: "after_change"` | Make the change named in `error.data`, then retry. |
| `retry: "unsafe"` | Do not retry automatically. Report the uncertain outcome. |
| `retry: "never"` | Stop or choose a different supported workflow. |
| `delivery: "uncertain"` | Do not repeat the Query, regardless of convenience. |
| exit `7` or `code: "recovery_required"` | Stop Workspace Applications and follow [MUTATIONS.md](MUTATIONS.md). |

Exit codes are coarse routing hints. The envelope is authoritative.

## Completion

A Query is complete when the requested semantic result is returned with its
qualifying metadata, or when the structured blocked or unsafe state has been
reported. A requested rename is complete after a matching Preview is applied
and its Receipt ID and filesystem state are retained.

Before publishing this skill with an `lspctl` release, run its examples against
that binary's `lspctl schema --full` output. Release validation must fail if a
literal command path, flag, contract version, error field, or enum used here is
absent.
