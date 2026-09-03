# Use a fixed static capability profile

`lspctl` will advertise one versioned LSP 3.17 Capability profile derived from the named operations and protocol handlers it implements, with selected 3.18 data understood only where required. The profile is fixed for a session, disables dynamic registration and configuration overrides, and also constrains raw requests; this avoids claiming editor behavior that `lspctl` cannot perform or maintaining mutable, server-specific capability state.
