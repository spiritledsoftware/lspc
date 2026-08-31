# Release dependency review

`deny.toml` checks the locked dependency graph for every supported release
target. Release dependencies must come from crates.io and have no applicable
RustSec advisory. The package is dual licensed `MIT OR Apache-2.0`.

## License decision

The currently locked graph is compatible with that distribution:

| SPDX expression or identifier | Decision |
| --- | --- |
| `MIT`, `MIT-0`, `Apache-2.0`, `Apache-2.0 WITH LLVM-exception`, `BSD-3-Clause` | Permissive; compatible. |
| `CC0-1.0`, `Unlicense` | Public-domain dedication; compatible. |
| `Unicode-3.0` | Permissive Unicode data license; compatible. |
| `MPL-2.0` | File-level copyleft in a dependency; compatible with distributing this binary under MIT OR Apache-2.0. |

`cargo deny check advisories bans licenses sources` is the recorded review
mechanism. Any unmaintained advisory needs a dated entry here with package,
advisory, impact, rationale, and decision before release; it is not silently
ignored in `deny.toml`.
