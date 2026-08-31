# Configuring language servers

Load this reference when the human asks to set up a language server for `lspc`.

`lspc` configures and launches an existing stdio language-server executable. It does not install or update language servers. If the executable is absent and the human asked for a complete setup, install it with the language server's supported package manager before configuring `lspc`. Keep that installation separate from the `lspc` configuration steps.

## Inspect the schemas

Run both configuration schema subjects before editing:

```sh
lspc schema config user
lspc schema config project
```

Use their fields, types, precedence rules, and resolved file locations as the authority. The user subject must report the current platform's resolved per-user configuration path so an Agent never guesses among Linux, macOS, and Windows conventions. Project configuration always lives at `.lspc.toml` in the canonical Workspace root.

Read an existing file before changing it. Make the smallest edit that adds or updates the requested server and preserve unrelated declarations. Both files are strict versioned TOML. Unknown fields, duplicate definitions, invalid types, and unsupported versions block use.

## Choose the configuration scope

- Use explicit invocation fields for a one-off server launch. They outrank files and create no persistent Trust grant.
- Use user configuration for reusable personal server commands, credentials, private environment values, and user-only session settings.
- Use project configuration for a shared Workspace route or declaration that belongs with the Workspace. Project-controlled launch fields require a Trust grant before execution.

Precedence is explicit invocation fields, project configuration, then user configuration. Omitted fields inherit. Lists replace. Ordinary maps merge one level. Initialization options and server settings replace as complete opaque values.

## Define the server

Build the declaration from the schema rather than copying a server-specific template. Establish only the fields the server needs:

- a stable server name;
- an executable as either a bare `PATH` name or a path containing a separator;
- separate arguments, with no shell command string;
- an optional working directory and environment overrides;
- optional initialization options, Workspace configuration settings, and request timeout;
- routes by exact language ID or file-extension suffix, plus an optional default for Workspace-wide Queries.

`lspc` performs no shell, environment-variable, or home-directory expansion. A path containing a separator resolves relative to the file that declared it. A bare name uses the owner's `PATH` and normal Windows executable resolution. The working directory defaults to the Workspace root.

Do not guess an installed executable or server-specific initialization value. Use values supplied by the human or the language server's own documentation. Keep secrets out of project configuration.

## Validate without bypassing Trust

After writing configuration:

1. Run the schema-declared `lspc trust status` form for the Workspace and server. This reloads configuration without starting the server. Invalid configuration fails; otherwise the command reports untrusted, changed, denied, or trusted state.
2. If project Trust is required, show the human the exact Workspace, server, digest, changed fields, and required command. Grant only after authorization, following [SKILL.md](SKILL.md).
3. Run `lspc capabilities` with the same Workspace and server. This verifies executable resolution, process launch, initialization, and normalized providers.
4. If the human requested working code intelligence rather than configuration alone, run one named Query against a matching Document.

Treat an `unsupported` provider as a valid server setup with that feature unavailable. Treat an `invalid` provider, launch failure, initialization failure, or routing ambiguity as an incomplete setup and report its structured error.

## Completion

Setup is complete when the intended configuration file contains a schema-valid declaration and route, Trust is in the intended state, and `lspc capabilities` initializes the selected server. Report the configuration path, Workspace, server name, Trust state, executable resolution, and provider states.
