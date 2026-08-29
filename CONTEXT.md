# Agent-facing LSP CLI

This context covers an agent's use of language-server code intelligence through shell commands.

## Language

**Agent**:
A software process that invokes shell commands and consumes structured results.
_Avoid_: User, client

**Query**:
An agent's request for semantic code intelligence from a language server.
_Avoid_: Command, operation

**Capability profile**:
The fixed client capabilities offered and server capabilities negotiated for one language-server session.
_Avoid_: Feature list, compatibility mode

**Owner**:
A long-lived process responsible for one initialized language-server session and its Queries.
_Avoid_: Daemon, broker

**Session identity**:
A stable identifier for one Workspace, selected server, and set of immutable language-server session inputs.
_Avoid_: Session ID, Owner ID

**Owner generation**:
One concrete lifetime of an Owner for a Session identity.
_Avoid_: Session identity, process ID

**Mutation**:
A language-server-proposed change to one or more files.
_Avoid_: Write, fix

**Preview**:
An identified, immutable representation of one Mutation that an Agent can inspect before authorizing it.
_Avoid_: Diff, plan

**Application**:
An Agent-authorized attempt to commit one Mutation to the filesystem.
_Avoid_: Execution, write

**Preauthorization**:
An Agent's advance authorization for Mutations requested while one language-server command runs.
_Avoid_: Blanket approval, trust

**Stale Preview**:
A Preview whose bound Workspace, server identity, authorization, or filesystem preconditions no longer match.
_Avoid_: Old preview, outdated edit

**Recovery**:
Resolution of a failed Application whose filesystem state could not be restored automatically.
_Avoid_: Retry, repair

**Receipt**:
A durable record of the terminal outcome of one Application or Recovery.
_Avoid_: Log, result

**Workspace**:
The filesystem tree presented to a language server as the context for queries and mutations.
_Avoid_: Project, repository

**Document**:
A filesystem-backed text file whose current snapshot has been presented to a language server.
_Avoid_: Buffer

**Trust grant**:
A user-local authorization for one Workspace's current named project server declaration to execute.
_Avoid_: Approval, allowlist

**Denial**:
A user-local decision that blocks one Workspace's named project server declaration until explicitly replaced by a Trust grant.
_Avoid_: Revocation
