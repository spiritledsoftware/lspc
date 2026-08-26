# Agent-facing LSP CLI

This context covers an agent's use of language-server code intelligence through shell commands.

## Language

**Agent**:
A software process that invokes shell commands and consumes structured results.
_Avoid_: User, client

**Query**:
An agent's request for semantic code intelligence from a language server.
_Avoid_: Command, operation

**Mutation**:
A language-server-proposed change to one or more files.
_Avoid_: Write, fix

**Workspace**:
The filesystem tree presented to a language server as the context for queries and mutations.
_Avoid_: Project, repository

**Trust grant**:
A user-local authorization for one Workspace's current named project server declaration to execute.
_Avoid_: Approval, allowlist

**Denial**:
A user-local decision that blocks one Workspace's named project server declaration until explicitly replaced by a Trust grant.
_Avoid_: Revocation
