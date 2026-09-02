# lspc

`lspc` is a JSON-only command-line client for querying installed Language Server Protocol servers and safely previewing their proposed filesystem changes. It keeps language servers warm between invocations and works on Linux, macOS, and Windows.

## Install

Install from a checkout with Rust 1.89 or newer:

```sh
cargo install --path . --locked
lspc version
```

`lspc` launches language servers but does not install them. Install the server you want to use separately.

## Configure a server

Find the native user configuration path:

```sh
lspc schema config user | jq -r .result.resolvedPath
```

Create that file with a server declaration and route. For example, if `rust-analyzer` is on `PATH`:

```toml
version = 1
default_server = "rust"

routes = [
  { server = "rust", language_id = "rust", extensions = [".rs"] },
]

[servers.rust]
executable = "rust-analyzer"
```

User configuration is trusted. A repository may instead commit `.lspc.toml`; project-controlled launch fields require an explicit `lspc trust grant` before the server can start.

## Query code

All output is one compact JSON object on stdout. Source lines and columns are zero-based Unicode-scalar positions.

```sh
lspc capabilities --workspace . --server rust
lspc definition --workspace . --server rust \
  --file src/lib.rs --line 12 --column 8
lspc references --workspace . --server rust \
  --file src/lib.rs --line 12 --column 8 --include-declaration true
```

A server may answer an early navigation Query before background indexing finishes. If an expected result is empty, inspect the optional `context.serverProgress`. Non-empty progress may be related, so watch it and retry after it clears:

```sh
lspc session status --workspace . --server rust | jq .result.progress
```

## Preview and apply a rename

Mutation Queries do not edit files. They persist an immutable Preview for inspection:

```sh
preview_id=$(
  lspc rename --workspace . --server rust \
    --file src/lib.rs --line 12 --column 8 --new-name replacement |
  jq -r .result.previewId
)
lspc preview show "$preview_id" | jq .result.diff
lspc apply "$preview_id"
```

Application rechecks the inspected Preview and records a durable Receipt. There is no force-apply path for stale edits.

## Discover commands

```sh
lspc help | jq
lspc help definition | jq
lspc schema definition | jq
lspc schema --full
```

`help` returns a compact command catalog without JSON Schemas. Focused `schema` returns the exact contract for one command; `schema --full` is the complete registry for tooling.

Use the structured `error.code`, `error.retry`, and `error.delivery` fields rather than parsing messages. See [`skills/lspc/`](skills/lspc/) for configuration, Query, Mutation, Trust, and Recovery workflows.

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.
