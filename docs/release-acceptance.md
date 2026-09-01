# Release acceptance

The CI matrix runs current stable and Rust 1.89 on Ubuntu, macOS, and Windows.
Each cell checks formatting, Clippy with warnings denied, tests, and that
`lspc schema --full` exactly exposes the checked contract assets. Product
checks have no retry step. A clean rerun is allowed only for an infrastructure
failure and must retain both attempts.

Linux also checks `cargo package --locked` and a locked source installation.
`cargo deny` blocks RustSec vulnerabilities, non-crates.io sources, and
unreviewed licenses for the five release targets. See
[dependency review](dependency-review.md).

## Candidate artifacts

`scripts/release/package.py` builds a target-native release binary and invokes
`build_archive.py`. The latter emits a deterministic target-named `.tar.gz`
or `.zip`, its SHA-256 sidecar, and a payload manifest. Archives contain the
binary, README, both licenses, `skills/lspc/`, and a manifest with the release
version, target, source commit, Rust version, skill digest, and per-file
checksums. The script rejects missing or symlinked skill payloads.

`build_skill_archive.py` creates a separate versioned companion-skill ZIP with
a checksum and skill/schema digests. `package.py --skill-only` derives the
schema from the built release binary before archiving the skill.

`release.yml` uses native runners for the five supported targets. It deliberately
does not cross-compile a release artifact and runs locked packaging, source
installation, and archive verification before uploading it. The candidate
workflow then reads `release-gates.json`; any pending gate fails the candidate.

## Pending gates

The repository is not release-ready. `release-gates.json` is the authoritative
machine-readable list: pinned reference-server smoke tests, native OS-floor checks,
stored-state compatibility, and performance/soak checks remain pending. They
must be implemented and marked `implemented`; they are never represented by
empty or passing placeholder tests.
