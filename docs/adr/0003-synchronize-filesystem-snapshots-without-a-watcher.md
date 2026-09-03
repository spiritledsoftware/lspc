# Synchronize filesystem snapshots without a watcher

The filesystem is authoritative, but persistent session owners need current Document state without inventing unsaved buffers. `lspctl` will keep a bounded set of Documents open, hash them before use, and replace changed snapshots with `didClose` followed by `didOpen`; it will not compute incremental edits or watch the Workspace in the MVP. This trades immediate notification of unopened-file changes for a smaller cross-platform client and leaves those files to the language server's filesystem handling.
