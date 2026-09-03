# Mutations with lspctl

Load this reference for any Query or server callback that can change Workspace resources.

## Obtain one Preview

Named `rename`, `format`, and a resolved code action containing an edit return either `unchanged` or a persisted Preview. They do not edit files.

`code-actions` returns the server's ordered list without persisting every embedded edit. Choose one complete Code Action object, then pass that object to the schema-declared `preview create` form. For an unwrapped method, pass the exact returned Workspace Edit and optional command to `preview create`. In both cases, bind the Preview with the originating `context.sessionIdentity` and `context.resultPositionEncoding`.

`execute-command` does not silently apply server-initiated `workspace/applyEdit` requests. By default, each valid request becomes a Preview listed in `applyEditLedger`, and the server receives `applied: false`. Use `--apply-edits` only when the Agent deliberately Preauthorizes those Applications for that one command and the human's request allows it. The ledger does not cover direct server side effects.

## Inspect before Application

Run the schema-validated form of:

```sh
lspctl preview show PREVIEW_ID
```

Inspect the lossless Workspace Edit, canonical plan, annotations, preconditions, conflicts, stale reasons, summary, and contextual diff. A Stale Preview may remain inspectable while its diff is unavailable.

An explicit request to rename, format, or apply a selected action authorizes only changes that match that request. Continue without another question when the Preview matches. Pause when it touches unexpected resources, contains unexplained resource operations, conflicts with the request, or lacks enough evidence to judge.

## Apply exactly what was inspected

Run the schema-validated form of:

```sh
lspctl apply PREVIEW_ID
```

Application revalidates the Preview's Workspace, server identity, authorization, and filesystem preconditions. There is no force, rebase, subset, or edit-at-apply shortcut.

If explicit invocation launch fields created the Preview, repeat those exact fields on `apply` as authorization evidence. They do not select or launch a server during the filesystem-local Application.

- On `applied` or `already_applied`, retain the Receipt ID and report the filesystem and session-synchronization outcome.
- On `stale`, discard or retain the old Preview for inspection, then rerun the originating Query to obtain a new proposal. Never force it through.
- On `rolled_back`, report that no proposed final state was committed and retain the Receipt.
- On `recovery_required`, stop all further Applications in that Workspace.

Preview creation alone is a valid stopping point when the human asked only to inspect a Mutation. Otherwise the workflow is complete after a successful Receipt or a reported structured failure.

## Recover before any other Application

When Recovery is required, run:

```sh
lspctl recovery status --workspace WORKSPACE
```

Report the transaction ID, manifest digest, affected resources, current state, and available schema-declared Recovery actions. Do not run `rollback` or `accept-current` without explicit human authorization for that transaction and digest. After Recovery, retain its Receipt and confirm that the Workspace write stop cleared before another Application.
