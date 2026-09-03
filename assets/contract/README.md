# Checked v1 contract assets

These files are based on the validated machine contract at commit `baaead1` and retain its v1 compatibility.

- `catalog.json` is the checked v1 catalog with its prototype marker removed.
- `schemas.json` is compiled from the checked catalog with that revision's `check.py` generator.
- `initialize-capabilities.json` is the exact Capability profile sent to language servers.

`lspctl` embeds these files. Builds do not run the prototype's Python generator.
