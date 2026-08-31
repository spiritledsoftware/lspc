# Checked v1 contract assets

These files come from the validated machine contract at commit `baaead1`.

- `catalog.json` is the frozen catalog with its prototype marker removed.
- `schemas.json` is the output of `check.py --emit-schemas` at that revision.
- `initialize-capabilities.json` is the exact Capability profile sent to language servers.

`lspc` embeds these files. Builds do not run the prototype's Python generator.
