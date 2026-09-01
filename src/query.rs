//! Pure named-Query composition and Owner-dispatch normalization.
//!
//! The Owner owns transport, synchronization, and lifecycle; this module owns
//! the stable translation between CLI inputs and LSP request/result JSON.

#![allow(clippy::result_large_err)]

use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
    path::PathBuf,
};

use serde_json::{Value, json};
use url::Url;

use crate::{
    cli::ParsedInvocation,
    contract::ContractFailure,
    workspace::{
        DiagnosticCache, DocumentSnapshot, PositionEncoding, ProtocolPosition,
        resolve_source_position,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryCommand {
    Definition,
    References,
    Hover,
    DocumentSymbols,
    WorkspaceSymbols,
    DocumentDiagnostics,
    WorkspaceDiagnostics,
    PublishedDiagnostics,
    PrepareRename,
    Rename,
    Format,
    CodeActions,
    ResolveCodeAction,
    ExecuteCommand,
    Raw,
    Capabilities,
}

impl QueryCommand {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Definition => "definition",
            Self::References => "references",
            Self::Hover => "hover",
            Self::DocumentSymbols => "document-symbols",
            Self::WorkspaceSymbols => "workspace-symbols",
            Self::DocumentDiagnostics => "document-diagnostics",
            Self::WorkspaceDiagnostics => "workspace-diagnostics",
            Self::PublishedDiagnostics => "published-diagnostics",
            Self::PrepareRename => "prepare-rename",
            Self::Rename => "rename",
            Self::Format => "format",
            Self::CodeActions => "code-actions",
            Self::ResolveCodeAction => "resolve-code-action",
            Self::ExecuteCommand => "execute-command",
            Self::Raw => "raw",
            Self::Capabilities => "capabilities",
        }
    }

    pub(crate) fn from_path(path: &[String]) -> Option<Self> {
        Some(match path {
            [name] if name == "definition" => Self::Definition,
            [name] if name == "references" => Self::References,
            [name] if name == "hover" => Self::Hover,
            [name] if name == "document-symbols" => Self::DocumentSymbols,
            [name] if name == "workspace-symbols" => Self::WorkspaceSymbols,
            [name] if name == "document-diagnostics" => Self::DocumentDiagnostics,
            [name] if name == "workspace-diagnostics" => Self::WorkspaceDiagnostics,
            [name] if name == "published-diagnostics" => Self::PublishedDiagnostics,
            [name] if name == "prepare-rename" => Self::PrepareRename,
            [name] if name == "rename" => Self::Rename,
            [name] if name == "format" => Self::Format,
            [name] if name == "code-actions" => Self::CodeActions,
            [name] if name == "resolve-code-action" => Self::ResolveCodeAction,
            [name] if name == "execute-command" => Self::ExecuteCommand,
            [name] if name == "raw" => Self::Raw,
            [name] if name == "capabilities" => Self::Capabilities,
            _ => return None,
        })
    }

    pub(crate) fn method(self) -> Option<&'static str> {
        Some(match self {
            Self::Definition => "textDocument/definition",
            Self::References => "textDocument/references",
            Self::Hover => "textDocument/hover",
            Self::DocumentSymbols => "textDocument/documentSymbol",
            Self::WorkspaceSymbols => "workspace/symbol",
            Self::DocumentDiagnostics => "textDocument/diagnostic",
            Self::WorkspaceDiagnostics => "workspace/diagnostic",
            Self::PublishedDiagnostics | Self::Capabilities => return None,
            Self::PrepareRename => "textDocument/prepareRename",
            Self::Rename => "textDocument/rename",
            Self::Format => "textDocument/formatting",
            Self::CodeActions => "textDocument/codeAction",
            Self::ResolveCodeAction => "codeAction/resolve",
            Self::ExecuteCommand => "workspace/executeCommand",
            Self::Raw => return None,
        })
    }

    fn provider(self) -> Option<ProviderRequirement> {
        Some(match self {
            Self::Definition => ProviderRequirement::new("definition", "definitionProvider"),
            Self::References => ProviderRequirement::new("references", "referencesProvider"),
            Self::Hover => ProviderRequirement::new("hover", "hoverProvider"),
            Self::DocumentSymbols => {
                ProviderRequirement::new("documentSymbols", "documentSymbolProvider")
            }
            Self::WorkspaceSymbols => {
                ProviderRequirement::new("workspaceSymbols", "workspaceSymbolProvider")
            }
            Self::DocumentDiagnostics | Self::WorkspaceDiagnostics => {
                ProviderRequirement::new("diagnostics", "diagnosticProvider")
            }
            Self::PrepareRename | Self::Rename => {
                ProviderRequirement::new("rename", "renameProvider")
            }
            Self::Format => ProviderRequirement::new("formatting", "documentFormattingProvider"),
            Self::CodeActions | Self::ResolveCodeAction => {
                ProviderRequirement::new("codeActions", "codeActionProvider")
            }
            Self::ExecuteCommand => {
                ProviderRequirement::new("executeCommand", "executeCommandProvider")
            }
            Self::PublishedDiagnostics | Self::Raw | Self::Capabilities => return None,
        })
    }

    fn supports_partial_results(self) -> bool {
        matches!(
            self,
            Self::References
                | Self::WorkspaceSymbols
                | Self::DocumentDiagnostics
                | Self::WorkspaceDiagnostics
                | Self::CodeActions
        )
    }

    fn paged(self) -> bool {
        matches!(
            self,
            Self::References
                | Self::WorkspaceSymbols
                | Self::DocumentDiagnostics
                | Self::WorkspaceDiagnostics
                | Self::PublishedDiagnostics
                | Self::CodeActions
        )
    }
}

#[derive(Debug, Clone, Copy)]
struct ProviderRequirement {
    key: &'static str,
    capability: &'static str,
}

impl ProviderRequirement {
    const fn new(key: &'static str, capability: &'static str) -> Self {
        Self { key, capability }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Provider {
    pub(crate) state: ProviderState,
    pub(crate) capability_path: String,
    pub(crate) options: Option<Value>,
    pub(crate) selector: Option<Value>,
    pub(crate) problems: Vec<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderState {
    Supported,
    Unsupported,
    Invalid,
}

impl ProviderState {
    fn name(self) -> &'static str {
        match self {
            Self::Supported => "supported",
            Self::Unsupported => "unsupported",
            Self::Invalid => "invalid",
        }
    }
}

#[derive(Debug, Default, Clone)]
pub(crate) struct Capabilities {
    pub(crate) providers: BTreeMap<String, Provider>,
    pub(crate) initialize_result: Option<Value>,
}

impl Capabilities {
    pub(crate) fn require(
        &self,
        command: QueryCommand,
        document: Option<&DocumentSnapshot>,
        invocation: &ParsedInvocation,
    ) -> Result<(), ContractFailure> {
        let Some(requirement) = command.provider() else {
            return Ok(());
        };
        let provider = self
            .providers
            .get(requirement.key)
            .or_else(|| self.providers.get(requirement.capability));
        let (state, capability_path, problems) = provider.map_or_else(
            || {
                (
                    "unsupported",
                    format!("capabilities.{}", requirement.capability),
                    Vec::new(),
                )
            },
            |provider| {
                (
                    provider.state.name(),
                    provider.capability_path.clone(),
                    provider.problems.clone(),
                )
            },
        );
        let unavailable =
            |selector: Option<Value>, state: &str, problems: Vec<Value>| ContractFailure {
                exit_code: 3,
                category: "blocked",
                code: "capability_unavailable",
                message: "The selected server does not support this Query.".to_owned(),
                stage: "dispatch",
                delivery: "not_sent",
                retry: "after_change",
                data: compact_object(json!({
                    "provider": requirement.key,
                    "state": state,
                    "capabilityPath": &capability_path,
                    "problems": problems,
                    "selector": selector,
                })),
            };
        let Some(provider) = provider else {
            return Err(unavailable(None, state, problems));
        };
        if provider.state != ProviderState::Supported {
            return Err(unavailable(provider.selector.clone(), state, problems));
        }
        let options = provider.options.as_ref().and_then(Value::as_object);
        let required_option = match command {
            QueryCommand::PrepareRename => Some(("prepareProvider", true)),
            QueryCommand::ResolveCodeAction => Some(("resolveProvider", true)),
            QueryCommand::WorkspaceDiagnostics => Some(("workspaceDiagnostics", true)),
            _ => None,
        };
        if let Some((name, expected)) = required_option
            && options
                .and_then(|options| options.get(name))
                .and_then(Value::as_bool)
                != Some(expected)
        {
            return Err(unavailable(
                provider.selector.clone(),
                "unsupported",
                Vec::new(),
            ));
        }
        if command == QueryCommand::ExecuteCommand {
            let wanted = invocation.option_string("--command").unwrap_or_default();
            let advertised = options
                .and_then(|options| options.get("commands"))
                .and_then(Value::as_array)
                .is_some_and(|commands| commands.iter().any(|command| command == &wanted));
            if !advertised {
                return Err(unavailable(
                    provider.selector.clone(),
                    "unsupported",
                    Vec::new(),
                ));
            }
        }
        let selector = provider
            .selector
            .as_ref()
            .or_else(|| options.and_then(|options| options.get("documentSelector")));
        if let (Some(selector), Some(document)) = (selector, document)
            && !selector_matches(selector, document)
        {
            return Err(unavailable(
                Some(selector.clone()),
                "unsupported",
                Vec::new(),
            ));
        }
        Ok(())
    }

    fn work_done_progress(&self, command: QueryCommand) -> bool {
        command
            .provider()
            .and_then(|requirement| {
                self.providers
                    .get(requirement.key)
                    .or_else(|| self.providers.get(requirement.capability))
            })
            .and_then(|provider| provider.options.as_ref())
            .and_then(|options| options.get("workDoneProgress"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DispatchRequest {
    pub(crate) method: String,
    /// `None` means omit JSON-RPC `params`; `Some(Value::Null)` is explicit null.
    pub(crate) params: Option<Value>,
    /// Native paths that the Owner must explicitly synchronize before raw dispatch.
    pub(crate) synchronized_files: Vec<PathBuf>,
    /// The Owner assigns a unique token and injects `partialResultToken` when true.
    pub(crate) partial_results: bool,
    /// The Owner assigns a unique token and injects `workDoneToken` when true.
    pub(crate) work_done_progress: bool,
    /// Whether the Owner should include exact protocol frames in its response.
    pub(crate) trace_protocol: bool,
    /// Whether execute-command callbacks may apply each proposed Workspace Edit.
    pub(crate) apply_edits: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchResponse {
    pub(crate) result: Value,
    pub(crate) partial_results: Vec<Value>,
    pub(crate) trace: Option<Value>,
    pub(crate) apply_edit_ledger: Vec<Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct QueryContext {
    pub(crate) workspace_uri: String,
    pub(crate) server: String,
    pub(crate) session_identity: String,
    pub(crate) owner_generation: String,
    pub(crate) result_position_encoding: PositionEncoding,
    pub(crate) synchronization: Value,
    pub(crate) recovery: Value,
}

impl QueryContext {
    fn json(&self, has_input_position: bool) -> Value {
        let mut context = json!({
            "workspaceUri": self.workspace_uri,
            "server": self.server,
            "sessionIdentity": self.session_identity,
            "ownerGeneration": self.owner_generation,
            "resultPositionEncoding": self.result_position_encoding.name(),
            "synchronization": self.synchronization,
            "recovery": self.recovery,
        });
        if has_input_position {
            context["inputPositionEncoding"] = Value::String("unicode-scalar".to_owned());
        }
        context
    }
}

/// The only seam required by the Query layer. The Owner implements this once.
pub(crate) trait SessionDispatcher {
    fn dispatch(&mut self, request: DispatchRequest) -> Result<DispatchResponse, ContractFailure>;
}

#[derive(Debug, Clone)]
pub(crate) struct ComposedQuery {
    pub(crate) command: QueryCommand,
    pub(crate) request: Option<DispatchRequest>,
    pub(crate) page: Option<Page>,
    pub(crate) document_uri: Option<String>,
    pub(crate) document_version: Option<i64>,
    capabilities: Option<Capabilities>,
    capabilities_raw: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) offset: usize,
    pub(crate) limit: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PreviewProposal {
    pub(crate) command: QueryCommand,
    pub(crate) method: String,
    pub(crate) context: Value,
    pub(crate) edit: Value,
    pub(crate) command_payload: Option<Value>,
    pub(crate) resolved_action: Option<Value>,
    pub(crate) trace: Option<Value>,
    pub(crate) apply_edit_ledger: Vec<Value>,
}

/// Mutation owns validation, persistence, and the complete Preview shape.
pub(crate) trait PreviewCreator {
    fn create_preview(&mut self, proposal: PreviewProposal) -> Result<Value, ContractFailure>;
}

/// Composes all named Query/raw request params from validated CLI options.
/// Documents have already been refreshed by the Owner and retain its negotiated encoding.
pub(crate) fn compose(
    invocation: &ParsedInvocation,
    document: Option<&DocumentSnapshot>,
    encoding: PositionEncoding,
    capabilities: &Capabilities,
    diagnostics: Option<&DiagnosticCache>,
) -> Result<ComposedQuery, ContractFailure> {
    let command = QueryCommand::from_path(invocation.command_path()).ok_or_else(unknown_query)?;
    let page = command.paged().then(|| page(invocation)).transpose()?;
    let document_uri = document.map(|document| document.uri.clone());
    if command != QueryCommand::Raw {
        capabilities.require(command, document, invocation)?;
    }
    let mut request = match command {
        QueryCommand::PublishedDiagnostics | QueryCommand::Capabilities => None,
        QueryCommand::Raw => Some(raw_request(invocation)?),
        QueryCommand::WorkspaceSymbols => Some(request(
            command.method().unwrap(),
            json!({"query": required_string(invocation, "--query")?}),
            Vec::new(),
        )),
        QueryCommand::WorkspaceDiagnostics => Some(request(
            command.method().unwrap(),
            json!({"previousResultIds": diagnostics.map_or_else(Vec::new, DiagnosticCache::pull_result_ids)}),
            Vec::new(),
        )),
        QueryCommand::DocumentSymbols => Some(document_request(
            command.method().unwrap(),
            required_document(document)?,
            None,
        )),
        QueryCommand::DocumentDiagnostics => {
            let document = required_document(document)?;
            let mut extra = json!({});
            if let Some(result_id) =
                diagnostics.and_then(|cache| cache.pull_result_id(&document.uri))
            {
                extra["previousResultId"] = Value::String(result_id.to_owned());
            }
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(extra),
            ))
        }
        QueryCommand::Definition | QueryCommand::Hover | QueryCommand::PrepareRename => {
            let document = required_document(document)?;
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(position_params(invocation, document, encoding)?),
            ))
        }
        QueryCommand::References => {
            let document = required_document(document)?;
            let mut params = position_params(invocation, document, encoding)?;
            params["context"] =
                json!({"includeDeclaration": required_bool(invocation, "--include-declaration")?});
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(params),
            ))
        }
        QueryCommand::Rename => {
            let document = required_document(document)?;
            let mut params = position_params(invocation, document, encoding)?;
            params["newName"] = json!(required_string(invocation, "--new-name")?);
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(params),
            ))
        }
        QueryCommand::Format => Some(document_request(
            command.method().unwrap(),
            required_document(document)?,
            Some(json!({"options": json_input(invocation, "--options-json", "--options-file")?})),
        )),
        QueryCommand::CodeActions => {
            let document = required_document(document)?;
            let range = range_params(invocation, document, encoding)?;
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(
                    json!({"range": range, "context": json_input(invocation, "--context-json", "--context-file")?}),
                ),
            ))
        }
        QueryCommand::ResolveCodeAction => Some(request(
            command.method().unwrap(),
            json_input(invocation, "--action-json", "--action-file")?,
            Vec::new(),
        )),
        QueryCommand::ExecuteCommand => Some(request(
            command.method().unwrap(),
            json!({
                "command": required_string(invocation, "--command")?,
                "arguments": optional_json_input(invocation, "--arguments-json", "--arguments-file")?.unwrap_or_else(|| json!([]))
            }),
            Vec::new(),
        )),
    };
    if let Some(request) = &mut request
        && command != QueryCommand::Raw
    {
        request.partial_results = command.supports_partial_results();
        request.work_done_progress = capabilities.work_done_progress(command);
    }
    if let Some(request) = &mut request {
        request.trace_protocol = invocation.has_option("--trace-protocol");
        request.apply_edits =
            command == QueryCommand::ExecuteCommand && invocation.has_option("--apply-edits");
    }
    Ok(ComposedQuery {
        command,
        request,
        page,
        document_uri,
        document_version: document.map(|document| document.version),
        capabilities: (command == QueryCommand::Capabilities).then(|| capabilities.clone()),
        capabilities_raw: command == QueryCommand::Capabilities && invocation.has_option("--raw"),
    })
}

/// Dispatches one composed LSP Query and produces a contract-shaped envelope.
pub(crate) fn execute(
    dispatcher: &mut impl SessionDispatcher,
    composed: ComposedQuery,
    context: QueryContext,
    diagnostics: &mut DiagnosticCache,
    previews: &mut impl PreviewCreator,
) -> Result<Value, ContractFailure> {
    let context = context.json(has_source_position(composed.command));
    match composed.command {
        QueryCommand::Capabilities => {
            return Ok(capabilities_envelope(
                context,
                composed.capabilities.as_ref().unwrap(),
                composed.capabilities_raw,
            ));
        }
        QueryCommand::PublishedDiagnostics => {
            return published_envelope(composed, context, diagnostics);
        }
        _ => {}
    }

    let response = dispatcher.dispatch(composed.request.clone().unwrap())?;
    if composed.command == QueryCommand::Raw {
        return query_envelope(
            &composed,
            context,
            response.result,
            response.trace,
            response.apply_edit_ledger,
        );
    }
    let result =
        merge_partial_results(composed.command, response.result, response.partial_results)?;
    let result = normalize_named_result(composed.command, result)?;

    match composed.command {
        QueryCommand::DocumentDiagnostics => document_diagnostic_envelope(
            &composed,
            context,
            result,
            response.trace,
            response.apply_edit_ledger,
            diagnostics,
        ),
        QueryCommand::WorkspaceDiagnostics => workspace_diagnostic_envelope(
            &composed,
            context,
            result,
            response.trace,
            response.apply_edit_ledger,
            diagnostics,
        ),
        QueryCommand::Rename | QueryCommand::Format | QueryCommand::ResolveCodeAction => {
            mutation_query_envelope(
                &composed,
                context,
                result,
                response.trace,
                response.apply_edit_ledger,
                previews,
            )
        }
        _ => query_envelope(
            &composed,
            context,
            result,
            response.trace,
            response.apply_edit_ledger,
        ),
    }
}

fn request(
    method: &str,
    params: Value,
    _already_synchronized_uris: Vec<String>,
) -> DispatchRequest {
    DispatchRequest {
        method: method.to_owned(),
        params: Some(params),
        synchronized_files: Vec::new(),
        partial_results: false,
        work_done_progress: false,
        trace_protocol: false,
        apply_edits: false,
    }
}

fn document_request(
    method: &str,
    document: &DocumentSnapshot,
    extra: Option<Value>,
) -> DispatchRequest {
    let mut params = extra.unwrap_or_else(|| json!({}));
    params["textDocument"] = json!({"uri": document.uri});
    request(method, params, vec![document.uri.clone()])
}

fn raw_request(invocation: &ParsedInvocation) -> Result<DispatchRequest, ContractFailure> {
    let method = required_string(invocation, "--method")?;
    if forbidden_raw_method(&method) {
        return Err(ContractFailure {
            exit_code: 3,
            category: "blocked",
            code: "raw_method_forbidden",
            message: "This raw method is controlled by lspc.".to_owned(),
            stage: "dispatch",
            delivery: "not_sent",
            retry: "never",
            data: json!({"method": method}),
        });
    }
    Ok(DispatchRequest {
        method,
        params: optional_json_input(invocation, "--params-json", "--params-file")?,
        synchronized_files: invocation.option_paths("--sync-file"),
        partial_results: false,
        work_done_progress: false,
        trace_protocol: false,
        apply_edits: false,
    })
}

fn forbidden_raw_method(method: &str) -> bool {
    matches!(
        method,
        "initialize" | "initialized" | "shutdown" | "exit" | "$/cancelRequest"
    )
}

fn compact_object(mut value: Value) -> Value {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, value| {
            !value.is_null() && !matches!(value, Value::Array(items) if items.is_empty())
        });
    }
    value
}

fn selector_matches(selector: &Value, document: &DocumentSnapshot) -> bool {
    let Some(filters) = selector.as_array() else {
        return false;
    };
    filters
        .iter()
        .any(|filter| document_filter_matches(filter, document))
}

fn document_filter_matches(filter: &Value, document: &DocumentSnapshot) -> bool {
    let Some(filter) = filter.as_object() else {
        return false;
    };
    if filter
        .get("language")
        .and_then(Value::as_str)
        .is_some_and(|language| language != document.language_id)
    {
        return false;
    }
    if filter
        .get("scheme")
        .and_then(Value::as_str)
        .is_some_and(|scheme| scheme != "file")
    {
        return false;
    }
    let Some(pattern) = filter.get("pattern").and_then(Value::as_str) else {
        return true;
    };
    let path = document.path.to_string_lossy().replace('\\', "/");
    glob_matches(pattern.as_bytes(), path.as_bytes())
}

fn glob_matches(pattern: &[u8], path: &[u8]) -> bool {
    let mut states = vec![vec![None; path.len() + 1]; pattern.len() + 1];
    glob_matches_at(pattern, path, 0, 0, &mut states)
}

fn glob_matches_at(
    pattern: &[u8],
    path: &[u8],
    pattern_index: usize,
    path_index: usize,
    states: &mut [Vec<Option<bool>>],
) -> bool {
    if let Some(result) = states[pattern_index][path_index] {
        return result;
    }
    let result = if pattern_index == pattern.len() {
        path_index == path.len()
    } else if pattern[pattern_index] == b'*' {
        let recursive = pattern.get(pattern_index + 1) == Some(&b'*');
        let next_pattern = pattern_index + if recursive { 2 } else { 1 };
        glob_matches_at(pattern, path, next_pattern, path_index, states)
            || (path_index < path.len()
                && (recursive || path[path_index] != b'/')
                && glob_matches_at(pattern, path, pattern_index, path_index + 1, states))
    } else if pattern[pattern_index] == b'?' {
        path_index < path.len()
            && path[path_index] != b'/'
            && glob_matches_at(pattern, path, pattern_index + 1, path_index + 1, states)
    } else {
        path.get(path_index) == pattern.get(pattern_index)
            && glob_matches_at(pattern, path, pattern_index + 1, path_index + 1, states)
    };
    states[pattern_index][path_index] = Some(result);
    result
}

fn position_params(
    invocation: &ParsedInvocation,
    document: &DocumentSnapshot,
    encoding: PositionEncoding,
) -> Result<Value, ContractFailure> {
    let position = source_position(invocation, document, encoding, "--line", "--column")?;
    Ok(json!({"position": position}))
}

fn range_params(
    invocation: &ParsedInvocation,
    document: &DocumentSnapshot,
    encoding: PositionEncoding,
) -> Result<Value, ContractFailure> {
    let start = source_position(
        invocation,
        document,
        encoding,
        "--start-line",
        "--start-column",
    )?;
    let end = source_position(invocation, document, encoding, "--end-line", "--end-column")?;
    if (end.line, end.character) < (start.line, start.character) {
        return Err(input_failure(
            "The code-action range ends before it starts.",
        ));
    }
    Ok(json!({"start": start, "end": end}))
}

fn source_position(
    invocation: &ParsedInvocation,
    document: &DocumentSnapshot,
    encoding: PositionEncoding,
    line: &str,
    column: &str,
) -> Result<ProtocolPosition, ContractFailure> {
    let line = u32::try_from(required_usize(invocation, line)?)
        .map_err(|_| input_failure("A source line is too large."))?;
    let column = u32::try_from(required_usize(invocation, column)?)
        .map_err(|_| input_failure("A source column is too large."))?;
    Ok(resolve_source_position(&document.path, &document.text, line, column, encoding)?.protocol)
}

fn page(invocation: &ParsedInvocation) -> Result<Page, ContractFailure> {
    let page = Page {
        offset: optional_usize(invocation, "--offset")?.unwrap_or(0),
        limit: optional_usize(invocation, "--limit")?.unwrap_or(100),
    };
    if !(1..=1000).contains(&page.limit) {
        return Err(input_failure("The page limit must be between 1 and 1000."));
    }
    Ok(page)
}

fn optional_usize(
    invocation: &ParsedInvocation,
    flag: &str,
) -> Result<Option<usize>, ContractFailure> {
    invocation
        .option_string(flag)
        .map(|value| {
            value
                .parse()
                .map_err(|_| input_failure("A numeric option is invalid."))
        })
        .transpose()
}

fn required_usize(invocation: &ParsedInvocation, flag: &str) -> Result<usize, ContractFailure> {
    optional_usize(invocation, flag)?.ok_or_else(|| input_failure("A required position is absent."))
}

fn required_string(invocation: &ParsedInvocation, flag: &str) -> Result<String, ContractFailure> {
    invocation
        .option_string(flag)
        .ok_or_else(|| input_failure("A required option is absent."))
}

fn required_bool(invocation: &ParsedInvocation, flag: &str) -> Result<bool, ContractFailure> {
    required_string(invocation, flag)?
        .parse()
        .map_err(|_| input_failure("A Boolean option is invalid."))
}

fn required_document(
    document: Option<&DocumentSnapshot>,
) -> Result<&DocumentSnapshot, ContractFailure> {
    document.ok_or_else(|| input_failure("A synchronized Document is required."))
}

fn json_input(
    invocation: &ParsedInvocation,
    json_flag: &str,
    file_flag: &str,
) -> Result<Value, ContractFailure> {
    optional_json_input(invocation, json_flag, file_flag)?
        .ok_or_else(|| input_failure("A JSON input source is required."))
}

fn optional_json_input(
    invocation: &ParsedInvocation,
    json_flag: &str,
    file_flag: &str,
) -> Result<Option<Value>, ContractFailure> {
    if let Some(value) = invocation.option_string(json_flag) {
        return serde_json::from_str(&value)
            .map(Some)
            .map_err(|error| invalid_json_input(json_flag, None, &error));
    }
    let Some(path) = invocation.option_path(file_flag) else {
        return Ok(None);
    };
    let contents = if path.as_os_str() == "-" {
        let mut contents = String::new();
        io::stdin()
            .read_to_string(&mut contents)
            .map_err(|error| input_read_failure(file_flag, None, &error))?;
        contents
    } else {
        fs::read_to_string(&path)
            .map_err(|error| input_read_failure(file_flag, Some(&path), &error))?
    };
    serde_json::from_str(&contents)
        .map_err(|error| invalid_json_input(file_flag, Some(&path), &error))
        .map(Some)
}

fn input_read_failure(source: &str, path: Option<&PathBuf>, error: &io::Error) -> ContractFailure {
    ContractFailure {
        exit_code: 2,
        category: "input",
        code: "input_read_failed",
        message: "A JSON input source could not be read.".to_owned(),
        stage: "read_input",
        delivery: "not_applicable",
        retry: "after_change",
        data: compact_object(json!({
            "source": source,
            "path": path.map(|path| path.to_string_lossy().into_owned()),
            "osCode": error.raw_os_error(),
        })),
    }
}

fn invalid_json_input(
    source: &str,
    path: Option<&PathBuf>,
    error: &serde_json::Error,
) -> ContractFailure {
    ContractFailure {
        exit_code: 2,
        category: "input",
        code: "invalid_json_input",
        message: "A JSON input source is invalid.".to_owned(),
        stage: "read_input",
        delivery: "not_applicable",
        retry: "never",
        data: compact_object(json!({
            "source": source,
            "path": path.map(|path| path.to_string_lossy().into_owned()),
            "line": error.line(),
            "column": error.column(),
        })),
    }
}

fn merge_partial_results(
    command: QueryCommand,
    mut result: Value,
    partials: Vec<Value>,
) -> Result<Value, ContractFailure> {
    if partials.is_empty() {
        return Ok(result);
    }
    match command {
        QueryCommand::References | QueryCommand::WorkspaceSymbols | QueryCommand::CodeActions => {
            let final_items = take_array_or_null(
                &mut result,
                command.method().unwrap(),
                "an array or null",
                "$",
            )?;
            let mut items = Vec::new();
            for (index, partial) in partials.into_iter().enumerate() {
                let Value::Array(mut partial) = partial else {
                    return Err(invalid_result(
                        command.method().unwrap(),
                        "an array partial result",
                        &format!("$partial[{index}]"),
                    ));
                };
                items.append(&mut partial);
            }
            items.extend(final_items);
            Ok(Value::Array(items))
        }
        QueryCommand::WorkspaceDiagnostics => {
            let Value::Object(mut final_report) = result else {
                return Err(invalid_result(
                    command.method().unwrap(),
                    "a WorkspaceDiagnosticReport object",
                    "$",
                ));
            };
            let mut items = Vec::new();
            for (index, partial) in partials.into_iter().enumerate() {
                let Some(mut partial_items) =
                    partial.get("items").and_then(Value::as_array).cloned()
                else {
                    return Err(invalid_result(
                        command.method().unwrap(),
                        "a WorkspaceDiagnosticReportPartialResult object",
                        &format!("$partial[{index}]"),
                    ));
                };
                items.append(&mut partial_items);
            }
            let Some(final_items) = final_report.get("items").and_then(Value::as_array) else {
                return Err(invalid_result(
                    command.method().unwrap(),
                    "an items array",
                    "$.items",
                ));
            };
            items.extend(final_items.iter().cloned());
            final_report.insert("items".to_owned(), Value::Array(items));
            Ok(Value::Object(final_report))
        }
        QueryCommand::DocumentDiagnostics => {
            let Value::Object(mut final_report) = result else {
                return Err(invalid_result(
                    command.method().unwrap(),
                    "a DocumentDiagnosticReport object",
                    "$",
                ));
            };
            let mut related = serde_json::Map::new();
            for (index, partial) in partials.into_iter().enumerate() {
                let Some(partial_related) =
                    partial.get("relatedDocuments").and_then(Value::as_object)
                else {
                    return Err(invalid_result(
                        command.method().unwrap(),
                        "a DocumentDiagnosticReportPartialResult object",
                        &format!("$partial[{index}]"),
                    ));
                };
                related.extend(partial_related.clone());
            }
            if let Some(final_related) = final_report
                .get("relatedDocuments")
                .and_then(Value::as_object)
            {
                related.extend(final_related.clone());
            }
            if !related.is_empty() {
                final_report.insert("relatedDocuments".to_owned(), Value::Object(related));
            }
            Ok(Value::Object(final_report))
        }
        _ => Err(invalid_result(
            command.method().unwrap_or("unknown"),
            "no partial results for this method",
            "$partial",
        )),
    }
}

fn take_array_or_null(
    value: &mut Value,
    method: &str,
    expected: &str,
    json_path: &str,
) -> Result<Vec<Value>, ContractFailure> {
    match std::mem::take(value) {
        Value::Array(items) => Ok(items),
        Value::Null => Ok(Vec::new()),
        _ => Err(invalid_result(method, expected, json_path)),
    }
}

fn normalize_named_result(
    command: QueryCommand,
    mut result: Value,
) -> Result<Value, ContractFailure> {
    let method = command.method().unwrap();
    match command {
        QueryCommand::Definition => {
            result = match result {
                Value::Null => Value::Array(Vec::new()),
                Value::Object(_) => Value::Array(vec![result]),
                Value::Array(_) => result,
                _ => {
                    return Err(invalid_result(
                        method,
                        "a Location, Location array, or null",
                        "$",
                    ));
                }
            };
            canonicalize_result_uris(command, &mut result)?;
        }
        QueryCommand::Hover | QueryCommand::PrepareRename => {
            result = match result {
                Value::Null => Value::Array(Vec::new()),
                Value::Object(_) => Value::Array(vec![result]),
                _ => return Err(invalid_result(method, "an object or null", "$")),
            };
        }
        QueryCommand::References
        | QueryCommand::DocumentSymbols
        | QueryCommand::WorkspaceSymbols
        | QueryCommand::CodeActions
        | QueryCommand::Format => {
            result = match result {
                Value::Null => Value::Array(Vec::new()),
                Value::Array(_) => result,
                _ => return Err(invalid_result(method, "an array or null", "$")),
            };
            canonicalize_result_uris(command, &mut result)?;
        }
        QueryCommand::DocumentDiagnostics | QueryCommand::WorkspaceDiagnostics => {
            if !result.is_object() {
                return Err(invalid_result(method, "a diagnostic report object", "$"));
            }
        }
        QueryCommand::Rename => {
            if !result.is_null() && !result.is_object() {
                return Err(invalid_result(
                    method,
                    "a WorkspaceEdit object or null",
                    "$",
                ));
            }
        }
        QueryCommand::ResolveCodeAction => {
            if !result.is_object() {
                return Err(invalid_result(method, "a CodeAction object", "$"));
            }
        }
        QueryCommand::ExecuteCommand => {}
        QueryCommand::Raw | QueryCommand::PublishedDiagnostics | QueryCommand::Capabilities => {
            unreachable!("local and raw Queries bypass named result normalization")
        }
    }
    Ok(result)
}

fn canonicalize_result_uris(
    command: QueryCommand,
    result: &mut Value,
) -> Result<(), ContractFailure> {
    let Some(items) = result.as_array_mut() else {
        return Ok(());
    };
    for (index, item) in items.iter_mut().enumerate() {
        match command {
            QueryCommand::Definition => {
                if item.get("targetUri").is_some() {
                    canonicalize_uri_field(
                        item,
                        "targetUri",
                        command.method().unwrap(),
                        &format!("$[{index}].targetUri"),
                    )?;
                } else {
                    canonicalize_uri_field(
                        item,
                        "uri",
                        command.method().unwrap(),
                        &format!("$[{index}].uri"),
                    )?;
                }
            }
            QueryCommand::References => canonicalize_uri_field(
                item,
                "uri",
                command.method().unwrap(),
                &format!("$[{index}].uri"),
            )?,
            QueryCommand::DocumentSymbols | QueryCommand::WorkspaceSymbols => {
                if let Some(location) = item.get_mut("location") {
                    canonicalize_uri_field(
                        location,
                        "uri",
                        command.method().unwrap(),
                        &format!("$[{index}].location.uri"),
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn canonicalize_uri_field(
    object: &mut Value,
    field: &str,
    method: &str,
    json_path: &str,
) -> Result<(), ContractFailure> {
    let Some(uri) = object.get_mut(field) else {
        return Err(invalid_result(method, "an LSP URI field", json_path));
    };
    let Some(uri_text) = uri.as_str() else {
        return Err(invalid_result(method, "a URI string", json_path));
    };
    let parsed =
        Url::parse(uri_text).map_err(|_| invalid_result(method, "a valid URI", json_path))?;
    if parsed.scheme() != "file" {
        return Ok(());
    }
    let path = parsed
        .to_file_path()
        .map_err(|()| invalid_result(method, "a valid file URI", json_path))?;
    *uri = Value::String(
        Url::from_file_path(path)
            .map_err(|()| invalid_result(method, "a valid file URI", json_path))?
            .to_string(),
    );
    Ok(())
}

fn query_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
) -> Result<Value, ContractFailure> {
    let command = command_name(composed.command);
    let method = composed
        .request
        .as_ref()
        .map(|request| request.method.as_str())
        .unwrap_or("textDocument/publishDiagnostics");
    let mut envelope = json!({
        "schemaVersion": 1,
        "ok": true,
        "command": [command],
        "method": method,
        "context": context,
        "result": result,
    });
    if let Some(page) = composed.page {
        add_page(&mut envelope, page, method)?;
    }
    if composed.command == QueryCommand::Raw {
        envelope["raw"] = Value::Bool(true);
    }
    if let Some(trace) = trace {
        envelope["trace"] = trace;
    }
    if !apply_edit_ledger.is_empty() || composed.command == QueryCommand::ExecuteCommand {
        envelope["applyEditLedger"] = Value::Array(apply_edit_ledger);
    }
    if composed.command == QueryCommand::ExecuteCommand {
        envelope["directServerSideEffects"] = Value::String("unknown".to_owned());
    }
    Ok(envelope)
}

#[allow(clippy::too_many_arguments)]
fn diagnostic_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    raw_report: Option<Value>,
    fresh: bool,
    complete: bool,
    workspace_complete: Option<bool>,
    source: &str,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
) -> Result<Value, ContractFailure> {
    let mut envelope = query_envelope(composed, context, result, trace, apply_edit_ledger)?;
    envelope["diagnostics"] = compact_object(json!({
        "source": source,
        "fresh": fresh,
        "complete": complete,
        "workspaceComplete": workspace_complete,
        "rawReport": raw_report,
    }));
    if workspace_complete.is_none() {
        envelope["diagnostics"]["workspaceComplete"] = Value::Null;
    }
    Ok(envelope)
}

fn document_diagnostic_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
    diagnostics: &mut DiagnosticCache,
) -> Result<Value, ContractFailure> {
    let uri = composed.document_uri.as_deref().unwrap();
    let diagnostic = diagnostics.apply_pull_report(uri, result);
    if diagnostic.effective_report.is_null() {
        return Err(invalid_result(
            composed.command.method().unwrap(),
            "a cached full report for an unchanged diagnostic result",
            "$",
        ));
    }
    let raw_report = (diagnostic.raw_report.get("kind").and_then(Value::as_str)
        == Some("unchanged"))
    .then_some(diagnostic.raw_report);
    diagnostic_envelope(
        composed,
        context,
        diagnostic.effective_report,
        raw_report,
        diagnostic.fresh,
        diagnostic.complete,
        None,
        "pull-document",
        trace,
        apply_edit_ledger,
    )
}

fn workspace_diagnostic_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
    diagnostics: &mut DiagnosticCache,
) -> Result<Value, ContractFailure> {
    let diagnostic = diagnostics.apply_workspace_pull_report(result);
    diagnostic_envelope(
        composed,
        context,
        diagnostic.effective_report,
        (!diagnostic.raw_report.is_null()).then_some(diagnostic.raw_report),
        diagnostic.fresh,
        diagnostic.complete,
        Some(diagnostic.workspace_complete),
        "pull-workspace",
        trace,
        apply_edit_ledger,
    )
}

fn mutation_query_envelope(
    composed: &ComposedQuery,
    context: Value,
    result: Value,
    trace: Option<Value>,
    apply_edit_ledger: Vec<Value>,
    previews: &mut impl PreviewCreator,
) -> Result<Value, ContractFailure> {
    if composed.command == QueryCommand::ResolveCodeAction
        && let Some(disabled) = result.get("disabled")
    {
        let reason = disabled
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("The selected code action is disabled.");
        return Err(ContractFailure {
            exit_code: 6,
            category: "mutation",
            code: "disabled_code_action",
            message: "The selected code action is disabled.".to_owned(),
            stage: "validate_mutation",
            delivery: "sent",
            retry: "never",
            data: json!({"reason": reason, "action": result}),
        });
    }

    let (edit, command_payload, resolved_action) = match composed.command {
        QueryCommand::Rename => (result, None, None),
        QueryCommand::Format => {
            let edits = result.as_array().unwrap();
            if edits.is_empty() {
                (Value::Null, None, None)
            } else {
                (
                    json!({
                        "documentChanges": [{
                            "textDocument": {
                                "uri": composed.document_uri,
                                "version": composed.document_version,
                            },
                            "edits": edits,
                        }]
                    }),
                    None,
                    None,
                )
            }
        }
        QueryCommand::ResolveCodeAction => (
            result.get("edit").cloned().unwrap_or(Value::Null),
            result.get("command").cloned(),
            Some(result),
        ),
        _ => unreachable!("only Mutation Queries create Previews"),
    };

    let preview = if edit.is_null() {
        Value::Null
    } else {
        previews.create_preview(PreviewProposal {
            command: composed.command,
            method: composed.request.as_ref().unwrap().method.clone(),
            context: context.clone(),
            edit,
            command_payload,
            resolved_action: resolved_action.clone(),
            trace: trace.clone(),
            apply_edit_ledger: apply_edit_ledger.clone(),
        })?
    };
    let outcome = if preview.is_null() {
        "unchanged"
    } else {
        "previewed"
    };
    let mut envelope = query_envelope(composed, context, preview, trace, apply_edit_ledger)?;
    envelope["outcome"] = Value::String(outcome.to_owned());
    if let Some(resolved_action) = resolved_action {
        envelope["resolvedAction"] = resolved_action;
    }
    Ok(envelope)
}

fn published_envelope(
    composed: ComposedQuery,
    context: Value,
    diagnostics: &mut DiagnosticCache,
) -> Result<Value, ContractFailure> {
    let (results, fresh, complete) = if let Some(uri) = &composed.document_uri {
        let current = diagnostics.published(uri, composed.document_version, true);
        (current.diagnostics, current.fresh, current.complete)
    } else {
        (
            json!(diagnostics.all_known().into_iter().map(|current| json!({"uri": current.uri, "version": current.version, "diagnostics": current.diagnostics, "fresh": current.fresh, "closed": current.closed})).collect::<Vec<_>>()),
            false,
            true,
        )
    };
    diagnostic_envelope(
        &composed,
        context,
        results,
        None,
        fresh,
        complete,
        None,
        "published",
        None,
        Vec::new(),
    )
}

fn capabilities_envelope(context: Value, capabilities: &Capabilities, include_raw: bool) -> Value {
    let providers = capabilities
        .providers
        .iter()
        .map(|(name, provider)| {
            (
                name.clone(),
                compact_object(json!({
                    "state": provider.state.name(),
                    "capabilityPath": provider.capability_path,
                    "options": provider.options,
                    "problems": provider.problems,
                })),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    let mut result = json!({
        "protocolBaseline": "3.17",
        "clientProfileVersion": 1,
        "providers": providers,
    });
    if include_raw && let Some(initialize_result) = &capabilities.initialize_result {
        result["initializeResult"] = initialize_result.clone();
    }
    json!({"schemaVersion": 1, "ok": true, "command": ["capabilities"], "context": context, "result": result})
}

fn add_page(envelope: &mut Value, page: Page, method: &str) -> Result<(), ContractFailure> {
    let result = &mut envelope["result"];
    let items = if result.is_array() {
        result.as_array_mut()
    } else {
        result.get_mut("items").and_then(Value::as_array_mut)
    };
    let Some(items) = items else {
        return Err(invalid_result(
            method,
            "a pageable top-level item collection",
            "$",
        ));
    };
    let total = items.len();
    let returned_items = items
        .drain(..)
        .skip(page.offset)
        .take(page.limit)
        .collect::<Vec<_>>();
    let returned = returned_items.len();
    *items = returned_items;
    let complete = page.offset.saturating_add(returned) >= total;
    envelope["page"] = json!({"offset": page.offset, "limit": page.limit, "returned": returned, "complete": complete, "nextOffset": (!complete).then_some(page.offset + returned)});
    Ok(())
}

fn has_source_position(command: QueryCommand) -> bool {
    matches!(
        command,
        QueryCommand::Definition
            | QueryCommand::References
            | QueryCommand::Hover
            | QueryCommand::PrepareRename
            | QueryCommand::Rename
            | QueryCommand::CodeActions
    )
}

fn command_name(command: QueryCommand) -> &'static str {
    match command {
        QueryCommand::Definition => "definition",
        QueryCommand::References => "references",
        QueryCommand::Hover => "hover",
        QueryCommand::DocumentSymbols => "document-symbols",
        QueryCommand::WorkspaceSymbols => "workspace-symbols",
        QueryCommand::DocumentDiagnostics => "document-diagnostics",
        QueryCommand::WorkspaceDiagnostics => "workspace-diagnostics",
        QueryCommand::PublishedDiagnostics => "published-diagnostics",
        QueryCommand::PrepareRename => "prepare-rename",
        QueryCommand::Rename => "rename",
        QueryCommand::Format => "format",
        QueryCommand::CodeActions => "code-actions",
        QueryCommand::ResolveCodeAction => "resolve-code-action",
        QueryCommand::ExecuteCommand => "execute-command",
        QueryCommand::Raw => "raw",
        QueryCommand::Capabilities => "capabilities",
    }
}

fn input_failure(message: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 2,
        category: "input",
        code: "invalid_arguments",
        message: message.to_owned(),
        stage: "parse_cli",
        delivery: "not_applicable",
        retry: "never",
        data: json!({"problems": []}),
    }
}

fn unknown_query() -> ContractFailure {
    input_failure("The command is not a Query.")
}

fn invalid_result(method: &str, expected: &str, json_path: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 5,
        category: "query",
        code: "invalid_server_result",
        message: format!("The {method} result did not match {expected}."),
        stage: "validate_result",
        delivery: "sent",
        retry: "after_change",
        data: json!({"method": method, "expected": expected, "jsonPath": json_path}),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{DocumentStore, TextSynchronization};
    use tempfile::TempDir;

    fn document() -> DocumentSnapshot {
        let root = TempDir::new().unwrap();
        let path = root.path().join("main.rs");
        std::fs::write(&path, "fn 😀() {}\n").unwrap();
        DocumentStore::new(2, 1024, 2048)
            .refresh(&path, "rust", TextSynchronization::OpenClose)
            .unwrap()
            .snapshot
    }

    fn supported(provider: &str) -> Capabilities {
        supported_with_options(provider, json!({}))
    }

    fn supported_with_options(provider: &str, options: Value) -> Capabilities {
        Capabilities {
            providers: BTreeMap::from([(
                provider.to_owned(),
                Provider {
                    state: ProviderState::Supported,
                    capability_path: provider.to_owned(),
                    options: Some(options),
                    selector: None,
                    problems: Vec::new(),
                },
            )]),
            initialize_result: None,
        }
    }

    fn context() -> QueryContext {
        QueryContext {
            workspace_uri: "file:///workspace/".to_owned(),
            server: "test".to_owned(),
            session_identity:
                "sid_0000000000000000000000000000000000000000000000000000000000000000".to_owned(),
            owner_generation: "gen_00000000000000000000000000000000".to_owned(),
            result_position_encoding: PositionEncoding::Utf16,
            synchronization: json!({"mode":"document", "bestEffort":false, "before":[], "failures":[], "postResponseChanged":[]}),
            recovery: json!({"required":false}),
        }
    }

    struct TestDispatcher(DispatchResponse);

    struct NoPreview;

    impl PreviewCreator for NoPreview {
        fn create_preview(&mut self, _proposal: PreviewProposal) -> Result<Value, ContractFailure> {
            Ok(Value::Null)
        }
    }

    impl SessionDispatcher for TestDispatcher {
        fn dispatch(
            &mut self,
            _request: DispatchRequest,
        ) -> Result<DispatchResponse, ContractFailure> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn named_position_requests_use_negotiated_encoding() {
        let document = document();
        let invocation = ParsedInvocation {
            command: vec!["definition".into()],
            options: BTreeMap::from([
                ("--line".into(), vec!["0".into()]),
                ("--column".into(), vec!["4".into()]),
            ]),
            positionals: Vec::new(),
        };
        let capabilities = supported("definitionProvider");
        let query = compose(
            &invocation,
            Some(&document),
            PositionEncoding::Utf16,
            &capabilities,
            None,
        )
        .unwrap();
        assert_eq!(
            query.request.unwrap().params.unwrap()["position"]["character"],
            5
        );
    }

    #[test]
    fn raw_preserves_explicit_null_and_sync_files() {
        let invocation = ParsedInvocation {
            command: vec!["raw".into()],
            options: BTreeMap::from([
                ("--method".into(), vec!["custom/method".into()]),
                ("--params-json".into(), vec!["null".into()]),
                ("--sync-file".into(), vec!["one.rs".into(), "two.rs".into()]),
            ]),
            positionals: Vec::new(),
        };
        let request = compose(
            &invocation,
            None,
            PositionEncoding::Utf16,
            &Capabilities::default(),
            None,
        )
        .unwrap()
        .request
        .unwrap();
        assert_eq!(request.params, Some(Value::Null));
        assert_eq!(
            request.synchronized_files,
            [PathBuf::from("one.rs"), PathBuf::from("two.rs")]
        );
    }

    #[test]
    fn unsupported_named_provider_fails_before_dispatch() {
        let invocation = ParsedInvocation {
            command: vec!["hover".into()],
            options: BTreeMap::new(),
            positionals: Vec::new(),
        };
        assert_eq!(
            compose(
                &invocation,
                None,
                PositionEncoding::Utf16,
                &Capabilities::default(),
                None,
            )
            .unwrap_err()
            .code,
            "capability_unavailable"
        );
    }

    #[test]
    fn partials_and_paging_are_normalized() {
        assert_eq!(
            merge_partial_results(QueryCommand::References, json!([3]), vec![json!([1, 2])])
                .unwrap(),
            json!([1, 2, 3])
        );
        let mut envelope = json!({"result": [0, 1, 2]});
        add_page(
            &mut envelope,
            Page {
                offset: 1,
                limit: 1,
            },
            "test/method",
        )
        .unwrap();
        assert_eq!(envelope["result"], json!([1]));
        assert_eq!(envelope["page"]["nextOffset"], 2);
    }

    #[test]
    fn code_action_and_execute_command_params_are_exact() {
        let document = document();
        let code_actions = ParsedInvocation {
            command: vec!["code-actions".into()],
            options: BTreeMap::from([
                ("--start-line".into(), vec!["0".into()]),
                ("--start-column".into(), vec!["0".into()]),
                ("--end-line".into(), vec!["0".into()]),
                ("--end-column".into(), vec!["2".into()]),
                (
                    "--context-json".into(),
                    vec![r#"{"diagnostics":[]}"#.into()],
                ),
            ]),
            positionals: Vec::new(),
        };
        let request = compose(
            &code_actions,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("codeActionProvider"),
            None,
        )
        .unwrap()
        .request
        .unwrap();
        assert_eq!(request.params.unwrap()["context"]["diagnostics"], json!([]));

        let command = ParsedInvocation {
            command: vec!["execute-command".into()],
            options: BTreeMap::from([
                ("--command".into(), vec!["test.run".into()]),
                ("--arguments-json".into(), vec!["[1]".into()]),
            ]),
            positionals: Vec::new(),
        };
        assert_eq!(
            compose(
                &command,
                None,
                PositionEncoding::Utf16,
                &supported_with_options(
                    "executeCommandProvider",
                    json!({"commands": ["test.run"]}),
                ),
                None,
            )
            .unwrap()
            .request
            .unwrap()
            .params
            .unwrap(),
            json!({"command":"test.run", "arguments":[1]})
        );
    }

    #[test]
    fn dispatch_normalizes_partials_context_and_document_diagnostics() {
        let document = document();
        let invocation = ParsedInvocation {
            command: vec!["document-diagnostics".into()],
            options: BTreeMap::new(),
            positionals: Vec::new(),
        };
        let query = compose(
            &invocation,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("diagnosticProvider"),
            None,
        )
        .unwrap();
        let mut dispatcher = TestDispatcher(DispatchResponse {
            result: json!({"kind":"full", "resultId":"one", "items":[{"message":"one"}]}),
            partial_results: Vec::new(),
            trace: None,
            apply_edit_ledger: Vec::new(),
        });
        let mut diagnostics = DiagnosticCache::new(4, 4096);
        let output = execute(
            &mut dispatcher,
            query,
            context(),
            &mut diagnostics,
            &mut NoPreview,
        )
        .unwrap();
        assert_eq!(output["context"]["inputPositionEncoding"], Value::Null);
        assert_eq!(
            output["result"],
            json!({"kind":"full", "resultId":"one", "items":[{"message":"one"}]})
        );
        assert_eq!(output["diagnostics"]["source"], "pull-document");
    }

    #[test]
    fn diagnostics_requests_reuse_cached_result_ids() {
        let document = document();
        let mut diagnostics = DiagnosticCache::new(4, 4096);
        diagnostics.apply_pull_report(
            &document.uri,
            json!({"kind":"full", "resultId":"old", "items":[]}),
        );
        let invocation = ParsedInvocation {
            command: vec!["document-diagnostics".into()],
            options: BTreeMap::new(),
            positionals: Vec::new(),
        };
        let request = compose(
            &invocation,
            Some(&document),
            PositionEncoding::Utf16,
            &supported("diagnosticProvider"),
            Some(&diagnostics),
        )
        .unwrap()
        .request
        .unwrap();
        assert_eq!(request.params.unwrap()["previousResultId"], "old");
    }

    #[test]
    fn raw_owner_methods_are_forbidden() {
        let invocation = ParsedInvocation {
            command: vec!["raw".into()],
            options: BTreeMap::from([("--method".into(), vec!["initialize".into()])]),
            positionals: Vec::new(),
        };
        assert_eq!(
            compose(
                &invocation,
                None,
                PositionEncoding::Utf16,
                &Capabilities::default(),
                None,
            )
            .unwrap_err()
            .code,
            "raw_method_forbidden"
        );
    }
}
