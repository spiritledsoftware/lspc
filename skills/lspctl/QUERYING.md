# Querying with lspctl

Load this reference for named Queries, diagnostics, paging, raw LSP methods, or protocol tracing.

## Prefer named Queries

Choose the named Query whose schema names the LSP method you need. Named Queries add synchronization, capability checks, bounded results, source conversion, and structured failures around one request. They are not compound workflows.

Examples of the shape, subject to the installed schema:

```sh
lspctl definition --workspace /work/repo --server rust --file src/lib.rs --line 12 --column 8
lspctl references --workspace /work/repo --server rust --file src/lib.rs --line 12 --column 8 --include-declaration false
```

Named source inputs use native file paths and zero-based `line` and `column`. The input column counts Unicode scalar values. Returned LSP positions retain the negotiated encoding named by `context.resultPositionEncoding`.

Never copy a returned LSP `character` into `--column`. Convert it against the current source line, or keep the whole follow-up in exact LSP form with `raw` when no named input conversion fits.

Read `context.synchronization` before using a result. A named Document Query that detects a post-response file change fails and places the unusable server result in structured error data. Do not treat that payload as current.

If a successful navigation Query unexpectedly returns an empty result, inspect `context.serverProgress`. A non-empty array means the server reported background work when it answered. Use `lspctl session status` for the same Workspace and server to watch `result.progress`; after it clears, repeat the Query. With no reported progress, treat the empty result as the server's answer.

## Page explicit results

Only use `--offset` and `--limit` when the leaf schema exposes them.

After a successful page:

1. Consume the returned items in server order.
2. Stop when `page.complete` is true or `page.nextOffset` is null.
3. Otherwise repeat the same explicit Query with `--offset` set to `page.nextOffset`.

Each page is a new Query. Do not claim snapshot consistency across pages, and do not fetch another page by repeating a failed request whose delivery is uncertain.

## Choose diagnostics by evidence needed

- Use `document-diagnostics` for a pull report about one current Document.
- Use `workspace-diagnostics` for a server-supported pull across the Workspace.
- Use `published-diagnostics` only for cached `textDocument/publishDiagnostics` notifications.

Inspect `diagnostics.source`, `fresh`, `complete`, and `workspaceComplete`. An empty result with `complete: false` does not mean zero diagnostics. `workspaceComplete: null` means completeness is unknowable. When a pull server returns an unchanged report, use the reconstructed result and retain `rawReport` for protocol diagnosis.

## Use raw for the long tail

Use `lspctl raw` only when the required LSP method has no named wrapper. Supply the exact method and exact JSON params defined by LSP or the server extension.

```sh
lspctl raw --workspace /work/repo --server rust --method textDocument/typeDefinition --params-file params.json --sync-file src/lib.rs
```

Raw mode performs no coordinate conversion, URI normalization, cardinality normalization, paging, or named-operation capability gate. `--sync-file` synchronizes a native file path before dispatch but does not transform params. Omitting params differs from sending JSON `null`. Lifecycle methods remain unavailable.

Treat `result` as exact server JSON. The outer envelope still supplies delivery, synchronization snapshots, post-response staleness, warnings, position encoding, and an optional trace. Do not use raw to bypass the safer wrapper for a named Query.

## Trace one Query

Add `--trace-protocol` to the initial Query when exact messages will help. If adding it after a failure, repeat the Query only when its retry and delivery fields permit repetition. Read the messages from the JSON envelope. Protocol trace data never belongs on stderr.

Trace output may contain source text, initialization data, file paths, and server messages. Keep it local unless the human asks to share it, and redact secrets before publication.
