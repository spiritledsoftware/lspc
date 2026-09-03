# Use per-session background owners

Independent CLI calls need to reuse initialized language-server state on Windows, macOS, and Linux. `lspctl` will automatically start one background owner per workspace, server, and effective configuration; clients connect through authenticated loopback TCP, operations run serially, and the owner exits when idle. This avoids repeated server startup, a global broker, platform-specific local IPC, and installed operating-system services while keeping session failures isolated.
