# lspc

[![Crates.io](https://img.shields.io/crates/v/lspc.svg)](https://crates.io/crates/lspc)
[![Acceptance](https://github.com/spiritledsoftware/lspc/actions/workflows/ci.yml/badge.svg)](https://github.com/spiritledsoftware/lspc/actions/workflows/ci.yml)
[![License](https://img.shields.io/crates/l/lspc.svg)](LICENSE-MIT)

**Language-server code intelligence for coding agents and shell scripts.**

`lspc` lets tools query installed Language Server Protocol servers without
embedding an editor. It returns one versioned JSON object per invocation and
keeps language servers warm between calls.

- Query definitions, references, hover, symbols, and diagnostics.
- Preview renames, formatting, code actions, and other filesystem changes
  before applying them.
- Reuse long-lived server sessions while keeping each CLI call scriptable.
- Inspect the complete command and output contract offline.
- Run on Linux, macOS, and Windows with any compatible language server.

## Install

Install from [crates.io](https://crates.io/crates/lspc) with Rust 1.89 or newer:

```sh
cargo install lspc --locked
lspc version | jq
```

Prebuilt archives and SHA-256 checksums are available on the
[Releases page](https://github.com/spiritledsoftware/lspc/releases).

`lspc` launches language servers but does not install them. Install the server
you want to use separately. The examples below also use
[`jq`](https://jqlang.org/) to format and select JSON output.

## Quick start

### 1. Configure a language server

Find the native user configuration path:

```sh
lspc schema config user | jq -r '.result.resolvedPath'
```

Create that file with a server declaration and route. For example, if
`rust-analyzer` is on `PATH`:

```toml
version = 1
default_server = "rust"

routes = [
  { server = "rust", language_id = "rust", extensions = [".rs"] },
]

[servers.rust]
executable = "rust-analyzer"
```

### 2. Query your code

Run these from a Rust workspace:

```sh
lspc capabilities --workspace . --server rust | jq '.result'

lspc definition --workspace . --server rust \
  --file src/main.rs --line 12 --column 8 | jq '.result'

lspc references --workspace . --server rust \
  --file src/main.rs --line 12 --column 8 \
  --include-declaration true | jq '.result'
```

Source lines and columns are zero-based Unicode-scalar positions. A server may
answer an early navigation query before background indexing finishes. Inspect
progress and retry after it clears when an expected result is empty:

```sh
lspc session status --workspace . --server rust | jq '.result.progress'
```

## Preview and apply changes

Mutation queries never edit files directly. They persist an immutable Preview
for inspection:

```sh
preview_id=$(
  lspc rename --workspace . --server rust \
    --file src/main.rs --line 12 --column 8 --new-name replacement |
  jq -r '.result.previewId'
)

lspc preview show "$preview_id" | jq -r '.result.diff'
lspc apply "$preview_id" | jq
```

Before applying, `lspc` rechecks the inspected Preview against the Workspace,
server configuration, authorization, and filesystem state. Every completed
Application records a durable Receipt; stale changes have no force-apply path.

## Project configuration and Trust

A repository may commit a `.lspc.toml` using the same `version`, `routes`, and
`servers` structure as the user configuration. Project-controlled server launch
fields require an explicit, declaration-bound Trust grant before execution.

When Trust is required, inspect the structured `project_trust_required` error
and run its `error.data.requiredCommand` only after approving the reported
Workspace, server, and declaration digest. User configuration and explicit CLI
launch fields do not require a project Trust grant.

## Discover the JSON contract

The CLI is JSON-only, including help and errors:

```sh
lspc help | jq                         # compact command index
lspc help definition | jq              # one command
lspc schema definition | jq            # exact input and output schemas
lspc schema --full > lspc-schema.json  # complete registry for tooling
```

Branch on stable fields such as `ok`, `error.code`, `error.retry`, and
`error.delivery` rather than parsing messages. The installed binary's schema is
the source of truth for its command syntax.

## Install the companion agent skill

Install the bundled workflow guidance into the current repository:

```sh
lspc skill install
```

This writes `.agent/skills/lspc`. Use `lspc skill install --global` to install
it under your home directory instead. Existing unmanaged files are not replaced
without `--replace`.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets --features fake-server -- -D warnings
cargo test --locked --all-targets --features fake-server
```

See the [v0.1.0 release notes](docs/releases/v0.1.0.md) for the tested platform
floors, reference-server versions, and artifact provenance.

## License

Licensed under either [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your
option.
