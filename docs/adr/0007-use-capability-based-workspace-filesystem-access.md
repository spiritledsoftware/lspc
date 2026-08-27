# Use capability-based Workspace filesystem access

Mutation planning and Application will open the canonical Workspace through `cap-std` and use `cap-fs-ext` for no-follow access and cross-platform resource identity. User-local state may use `atomic-write-file` for locked single-record replacement, but the Mutation module will own its multi-resource journal, staging, rollback, and Recovery because no general-purpose crate provides the settled semantics. This concentrates path confinement and filesystem race handling at one seam instead of repeating lexical path checks across callers.
