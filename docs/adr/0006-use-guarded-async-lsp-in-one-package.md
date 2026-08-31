# Use guarded async-lsp in one package

`lspc` will be one Cargo package whose main executable also runs its hidden background-owner mode. The session module will own bounded JSON-RPC framing, serialization, request identifiers, response correlation, and cancellation so raw requests preserve the difference between omitted parameters and explicit `null`. It will use `async-lsp` for request routing, middleware, and server callbacks, but not its transport main loop. This avoids a fork or internal crate workspace while keeping the protocol contract and safety limits under `lspc`'s control.
