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
