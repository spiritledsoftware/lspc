//! Pure named-Query composition and Owner-dispatch normalization.
//!
//! The Owner owns transport, synchronization, and lifecycle; this module owns
//! the stable translation between CLI inputs and LSP request/result JSON.

#![allow(clippy::result_large_err)]

use std::{collections::BTreeMap, fs};

use serde_json::{Value, json};

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

    fn provider(self) -> Option<&'static str> {
        Some(match self {
            Self::Definition => "definitionProvider",
            Self::References => "referencesProvider",
            Self::Hover => "hoverProvider",
            Self::DocumentSymbols => "documentSymbolProvider",
            Self::WorkspaceSymbols => "workspaceSymbolProvider",
            Self::DocumentDiagnostics | Self::WorkspaceDiagnostics => "diagnosticProvider",
            Self::PrepareRename | Self::Rename => "renameProvider",
            Self::Format => "documentFormattingProvider",
            Self::CodeActions | Self::ResolveCodeAction => "codeActionProvider",
            Self::ExecuteCommand => "executeCommandProvider",
            Self::PublishedDiagnostics | Self::Raw | Self::Capabilities => return None,
        })
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

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Provider {
    pub(crate) state: ProviderState,
    pub(crate) capability_path: String,
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
}

impl Capabilities {
    pub(crate) fn require(&self, command: QueryCommand) -> Result<(), ContractFailure> {
        self.require_provider(command.provider())
    }

    fn require_method(&self, method: &str) -> Result<(), ContractFailure> {
        self.require_provider(raw_provider(method))
    }

    fn require_provider(&self, provider_name: Option<&str>) -> Result<(), ContractFailure> {
        let Some(name) = provider_name else {
            return Ok(());
        };
        let provider = self.providers.get(name);
        if provider.is_some_and(|provider| provider.state == ProviderState::Supported) {
            return Ok(());
        }
        let (state, capability_path, problems) =
            provider.map_or(("unsupported", "", Vec::new()), |provider| {
                (
                    provider.state.name(),
                    provider.capability_path.as_str(),
                    provider.problems.clone(),
                )
            });
        Err(ContractFailure {
            exit_code: 3,
            category: "blocked",
            code: "capability_unavailable",
            message: "The selected server does not support this Query.".to_owned(),
            stage: "dispatch",
            delivery: "not_sent",
            retry: "after_change",
            data: json!({"provider": name, "state": state, "capabilityPath": capability_path, "problems": problems}),
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct DispatchRequest {
    pub(crate) method: String,
    /// `None` means omit JSON-RPC `params`; `Some(Value::Null)` is explicit null.
    pub(crate) params: Option<Value>,
    pub(crate) synchronized_uris: Vec<String>,
    pub(crate) partial_result_token: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct DispatchResponse {
    pub(crate) result: Value,
    pub(crate) partial_results: Vec<Value>,
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
    capabilities: Option<Capabilities>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Page {
    pub(crate) offset: usize,
    pub(crate) limit: usize,
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
    capabilities.require(command)?;
    let page = command.paged().then(|| page(invocation)).transpose()?;
    let document_uri = document.map(|document| document.uri.clone());
    let request = match command {
        QueryCommand::PublishedDiagnostics | QueryCommand::Capabilities => None,
        QueryCommand::Raw => {
            let request = raw_request(invocation)?;
            capabilities.require_method(&request.method)?;
            Some(request)
        }
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
            Some(document_request(
                command.method().unwrap(),
                document,
                Some(json!({
                    "identifier": null,
                    "previousResultId": diagnostics.and_then(|cache| cache.pull_result_id(&document.uri))
                })),
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
    Ok(ComposedQuery {
        command,
        request,
        page,
        document_uri,
        capabilities: (command == QueryCommand::Capabilities).then(|| capabilities.clone()),
    })
}

/// Dispatches one composed LSP Query and produces a contract-shaped envelope.
pub(crate) fn execute(
    dispatcher: &mut impl SessionDispatcher,
    composed: ComposedQuery,
    context: QueryContext,
    diagnostics: &mut DiagnosticCache,
) -> Result<Value, ContractFailure> {
    let context = context.json(has_source_position(composed.command));
    let result = match composed.command {
        QueryCommand::Capabilities => {
            return Ok(capabilities_envelope(
                context,
                composed.capabilities.as_ref().unwrap(),
            ));
        }
        QueryCommand::PublishedDiagnostics => {
            return published_envelope(composed, context, diagnostics);
        }
        _ => {
            let response = dispatcher.dispatch(composed.request.clone().unwrap())?;
            merge_partial_results(response.result, response.partial_results, 64 * 1024 * 1024)?
        }
    };
    if let Some(uri) = &composed.document_uri
        && composed.command == QueryCommand::DocumentDiagnostics
    {
        let diagnostic = diagnostics.apply_pull_report(uri, result.clone());
        return Ok(diagnostic_envelope(
            composed,
            context,
            diagnostic.diagnostics,
            diagnostic.raw_report,
            diagnostic.fresh,
            diagnostic.complete,
            "pull-document",
        ));
    }
    Ok(query_envelope(composed, context, result))
}

fn request(method: &str, params: Value, synchronized_uris: Vec<String>) -> DispatchRequest {
    DispatchRequest {
        method: method.to_owned(),
        params: Some(params),
        synchronized_uris,
        partial_result_token: None,
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
        synchronized_uris: invocation.option_strings("--sync-file").unwrap_or_default(),
        partial_result_token: None,
    })
}

fn forbidden_raw_method(method: &str) -> bool {
    matches!(
        method,
        "initialize"
            | "shutdown"
            | "exit"
            | "$/cancelRequest"
            | "$/progress"
            | "workspace/applyEdit"
            | "workspace/configuration"
            | "client/registerCapability"
            | "client/unregisterCapability"
    )
}

fn raw_provider(method: &str) -> Option<&'static str> {
    match method {
        "textDocument/definition" => Some("definitionProvider"),
        "textDocument/references" => Some("referencesProvider"),
        "textDocument/hover" => Some("hoverProvider"),
        "textDocument/documentSymbol" => Some("documentSymbolProvider"),
        "workspace/symbol" => Some("workspaceSymbolProvider"),
        "textDocument/diagnostic" | "workspace/diagnostic" => Some("diagnosticProvider"),
        "textDocument/prepareRename" | "textDocument/rename" => Some("renameProvider"),
        "textDocument/formatting" => Some("documentFormattingProvider"),
        "textDocument/codeAction" | "codeAction/resolve" => Some("codeActionProvider"),
        "workspace/executeCommand" => Some("executeCommandProvider"),
        _ => None,
    }
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
    Ok(Page {
        offset: optional_usize(invocation, "--offset")?.unwrap_or(0),
        limit: optional_usize(invocation, "--limit")?.unwrap_or(100),
    })
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
            .map_err(|_| input_failure("JSON input is invalid."));
    }
    let Some(path) = invocation.option_path(file_flag) else {
        return Ok(None);
    };
    fs::read_to_string(path)
        .map_err(|_| input_failure("JSON input cannot be read."))
        .and_then(|value| {
            serde_json::from_str(&value).map_err(|_| input_failure("JSON input is invalid."))
        })
        .map(Some)
}

fn merge_partial_results(
    mut result: Value,
    partials: Vec<Value>,
    max_bytes: usize,
) -> Result<Value, ContractFailure> {
    if partials.is_empty() {
        return Ok(result);
    }
    let mut items = match result {
        Value::Array(items) => items,
        Value::Null => Vec::new(),
        _ => {
            return Err(invalid_result(
                "A partial-result Query must return an array.",
            ));
        }
    };
    for partial in partials {
        let Value::Array(mut partial) = partial else {
            return Err(invalid_result("A partial result must be an array."));
        };
        items.append(&mut partial);
        if serde_json::to_vec(&items).map_or(true, |bytes| bytes.len() > max_bytes) {
            return Err(ContractFailure {
                exit_code: 5,
                category: "query",
                code: "partial_result_too_large",
                message: "Partial results exceed the configured limit.".to_owned(),
                stage: "await_response",
                delivery: "sent",
                retry: "never",
                data: json!({"limitBytes": max_bytes}),
            });
        }
    }
    result = Value::Array(items);
    Ok(result)
}

fn query_envelope(composed: ComposedQuery, context: Value, result: Value) -> Value {
    let command = command_name(composed.command);
    let mut envelope = json!({
        "schemaVersion": 1,
        "ok": true,
        "command": [command],
        "method": composed.request.as_ref().unwrap().method,
        "context": context,
        "result": result,
    });
    if let Some(page) = composed.page {
        add_page(&mut envelope, page);
    }
    if composed.command == QueryCommand::Raw {
        envelope["raw"] = Value::Bool(true);
    }
    if composed.command == QueryCommand::ExecuteCommand {
        envelope["applyEditLedger"] = json!([]);
        envelope["directServerSideEffects"] = Value::String("unknown".to_owned());
    }
    envelope
}

fn diagnostic_envelope(
    composed: ComposedQuery,
    context: Value,
    result: Value,
    raw_report: Value,
    fresh: bool,
    complete: bool,
    source: &str,
) -> Value {
    let mut envelope = query_envelope(composed, context, result);
    envelope["diagnostics"] = json!({
        "source": source,
        "fresh": fresh,
        "complete": complete,
        "workspaceComplete": Value::Null,
        "rawReport": raw_report,
    });
    envelope
}

fn published_envelope(
    composed: ComposedQuery,
    context: Value,
    diagnostics: &mut DiagnosticCache,
) -> Result<Value, ContractFailure> {
    let results = if let Some(uri) = &composed.document_uri {
        let current = diagnostics.published(uri, None, true);
        json!([{"uri": current.uri, "version": current.version, "diagnostics": current.diagnostics, "fresh": current.fresh, "closed": current.closed}])
    } else {
        json!(diagnostics.all_known().into_iter().map(|current| json!({"uri": current.uri, "version": current.version, "diagnostics": current.diagnostics, "fresh": current.fresh, "closed": current.closed})).collect::<Vec<_>>())
    };
    Ok(diagnostic_envelope(
        composed,
        context,
        results,
        Value::Null,
        false,
        true,
        "published",
    ))
}

fn capabilities_envelope(context: Value, capabilities: &Capabilities) -> Value {
    let providers = capabilities
        .providers
        .iter()
        .map(|(name, provider)| {
            (
                name.clone(),
                json!({
                    "state": provider.state.name(),
                    "capabilityPath": provider.capability_path,
                    "problems": provider.problems,
                }),
            )
        })
        .collect::<serde_json::Map<_, _>>();
    json!({"schemaVersion": 1, "ok": true, "command": ["capabilities"], "context": context, "result": {"protocolBaseline": "3.17", "clientProfileVersion": 1, "providers": providers}})
}

fn add_page(envelope: &mut Value, page: Page) {
    let result = &mut envelope["result"];
    let items = if result.is_array() {
        result.as_array_mut()
    } else {
        result.get_mut("items").and_then(Value::as_array_mut)
    };
    let Some(items) = items else {
        envelope["page"] = json!({"offset": page.offset, "limit": page.limit, "returned": 0, "complete": true, "nextOffset": Value::Null});
        return;
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
        stage: "read_input",
        delivery: "not_applicable",
        retry: "never",
        data: json!({"problems": []}),
    }
}

fn unknown_query() -> ContractFailure {
    input_failure("The command is not a Query.")
}

fn invalid_result(message: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 5,
        category: "query",
        code: "invalid_server_result",
        message: message.to_owned(),
        stage: "validate_result",
        delivery: "sent",
        retry: "never",
        data: json!({}),
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
        Capabilities {
            providers: BTreeMap::from([(
                provider.to_owned(),
                Provider {
                    state: ProviderState::Supported,
                    capability_path: provider.to_owned(),
                    problems: Vec::new(),
                },
            )]),
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
        assert_eq!(request.synchronized_uris, ["one.rs", "two.rs"]);
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
    fn partials_are_bounded_and_paging_is_normalized() {
        assert_eq!(
            merge_partial_results(json!([1]), vec![json!([2, 3])], 100).unwrap(),
            json!([1, 2, 3])
        );
        let mut envelope = json!({"result": [0, 1, 2]});
        add_page(
            &mut envelope,
            Page {
                offset: 1,
                limit: 1,
            },
        );
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
                &supported("executeCommandProvider"),
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
        });
        let mut diagnostics = DiagnosticCache::new(4, 4096);
        let output = execute(&mut dispatcher, query, context(), &mut diagnostics).unwrap();
        assert_eq!(output["context"]["inputPositionEncoding"], Value::Null);
        assert_eq!(output["result"], json!([{"message":"one"}]));
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
