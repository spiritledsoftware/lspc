---
name: lspc
description: Use the lspc CLI for language-server Queries and Mutations. Invoke for definitions, references, symbols, diagnostics, rename, formatting, code actions, raw LSP methods, server configuration, project Trust, protocol tracing, or Recovery.
---

# lspc

Use `lspc` as a schema-driven LSP client. The schema defines syntax. This skill defines the workflow.

## Establish the contract

Before the first LSP workflow in a session, run:

```sh
lspc schema
```

Keep this compact catalog for the session. Require a successful envelope with `schemaVersion: 1` and `result.contractVersion: 1`.

Before invoking a leaf command, find its path in that catalog and validate its flags, required values, exclusions, success schema, error schema, and exit codes. A focused `lspc schema <command path>` lookup is enough after the catalog has established the contract.

The installed binary's schema wins if it disagrees with this skill. Report the mismatch and stop instead of inventing an alias, flag, default, or fallback. `lspc schema` is offline, so schema discovery never needs configuration, Trust, or a running language server.

## Choose the workflow

- For a named Query, diagnostics, paging, an unwrapped LSP method, or protocol tracing, read [QUERYING.md](QUERYING.md).
- For rename, formatting, code actions, `workspace/applyEdit`, Preview inspection, Application, Receipts, or Recovery, read [MUTATIONS.md](MUTATIONS.md).
- When the human asks to add, configure, route, or verify a language server, read [CONFIGURATION.md](CONFIGURATION.md).
- For routine server selection, Trust, capabilities, and structured errors, continue here.

## Select the Workspace and server

Use one canonical Workspace root and one named server for a related sequence of commands. Repeat the same `--workspace` and `--server` selection when the current directory and language routing do not identify them unambiguously.

For configuration work, follow [CONFIGURATION.md](CONFIGURATION.md). Pass explicit server launch fields only when the task already establishes them. Invocation-scoped launch fields create no persistent Trust grant.

Project launch fields require a declaration-bound Trust grant. User configuration and explicit invocation fields do not. Use `lspc trust status` to inspect the current state before changing it. A grant authorizes the current declaration digest, `trust revoke` removes either a grant or a Denial, and a Denial keeps the declaration blocked until explicitly replaced.

When an error has `code: "project_trust_required"`:

1. Read the Workspace URI, server, declaration digest, and `requiredCommand` from `error.data`.
2. Show those exact details to the human.
3. Run the supplied Trust command only after the human authorizes that Workspace, server, and digest.

Do not broaden a server grant to `trust grant --all`. A durable Denial may be replaced only when the human explicitly authorizes the schema-declared replacement flag.

Run `lspc capabilities` when server support matters. Use normalized provider states of `supported`, `unsupported`, and `invalid`. The raw initialization result is diagnostic data, not a substitute for those gates.

## Process every envelope

Parse stdout as one JSON object. Use stderr only when `lspc` could not serialize an envelope.

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

A Query is complete when the requested semantic result is returned with its qualifying metadata, or when the structured blocked or unsafe state has been reported. A Mutation is complete only at the stopping point defined in [MUTATIONS.md](MUTATIONS.md).

Before publishing this skill with an `lspc` release, run its examples against that binary's `lspc schema --full` output. Release validation must fail if a literal command path, flag, contract version, error field, or enum used here is absent.
