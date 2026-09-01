use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io,
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

use ::time::{OffsetDateTime, format_description::well_known::Rfc3339};
use async_lsp::{AnyRequest, ErrorCode, ResponseError, router::Router};
use atomic_write_file::AtomicWriteFile;
use fs2::FileExt;
use serde_json::{Value, json};
use tokio::{
    io::AsyncReadExt,
    net::{TcpListener, TcpStream},
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{self as tokio_time, Instant as TokioInstant},
};
use tower_service::Service;
use url::Url;

use super::{
    capabilities::{
        CAPABILITY_PROFILE_VERSION, NegotiatedCapabilities, fixed_initialize_capabilities,
        normalize_initialize_result,
    },
    json_rpc_transport::{JsonRpcFrame, JsonRpcFrameReader, JsonRpcFrameWriter},
    owner_protocol::{
        AuthenticatedOwnerRequest, OWNER_PROTOCOL_VERSION, OWNER_QUEUE_LIMIT, OwnerDocumentInput,
        OwnerEndpoint, OwnerLaunchSettings, OwnerRequest, OwnerResponse,
        constant_time_token_matches, read_owner_message, write_owner_message,
    },
    process_supervision::SupervisedServerProcess,
    session_log::SessionLog,
};
use crate::{
    contract::ContractFailure,
    workspace::{DiagnosticCache, DocumentStore, SynchronizationEvent},
};

const TRACE_CAPACITY_BYTES: usize = 64 * 1024 * 1024;

pub(crate) struct OwnerBootstrap {
    pub(crate) session_identity: String,
    pub(crate) owner_generation: String,
    pub(crate) token: String,
    pub(crate) workspace_uri: String,
    pub(crate) workspace_path: PathBuf,
    pub(crate) server: String,
    pub(crate) executable: PathBuf,
    pub(crate) endpoint_path: PathBuf,
    pub(crate) owner_lock_path: PathBuf,
    pub(crate) launch_path: PathBuf,
}

struct PendingOwnerRequest {
    request: OwnerRequest,
    response: oneshot::Sender<OwnerResponse>,
    delivered: oneshot::Receiver<()>,
    cancelled: watch::Receiver<bool>,
}

struct ActiveQuery {
    method: String,
    params: Option<Value>,
    started_at: String,
    response: Option<oneshot::Sender<OwnerResponse>>,
    cancelled: watch::Receiver<bool>,
    deadline: TokioInstant,
    timeout: Duration,
    cancellation: Option<QueryCancellation>,
    partial_token: Option<Value>,
    partial_items: Vec<Value>,
    partial_bytes: usize,
    trace: Option<ProtocolTrace>,
    documents: Vec<OwnerDocumentInput>,
    apply_edits: bool,
    apply_edit_ledger: Vec<Value>,
    synchronization: Value,
}

struct QueryCancellation {
    deadline: TokioInstant,
    failure: Value,
}

struct LspRuntime {
    process: SupervisedServerProcess,
    reader: JsonRpcFrameReader<tokio::process::ChildStdout>,
    writer: JsonRpcFrameWriter<tokio::process::ChildStdin>,
    stderr: mpsc::Receiver<String>,
    next_request_id: i64,
    negotiated: NegotiatedCapabilities,
    diagnostics: DiagnosticCache,
    documents: DocumentStore,
    progress: BTreeMap<String, Value>,
    settings: Value,
    workspace_uri: String,
    workspace_path: PathBuf,
    server: String,
    session_identity: String,
    workspace_folder: Value,
    cancellation_grace: Duration,
    shutdown_timeout: Duration,
    max_partial_result_bytes: usize,
    log: SessionLog,
    startup_trace: Option<ProtocolTrace>,
    preview_settings: crate::configuration::PreviewSettings,
    receipt_settings: crate::configuration::ReceiptSettings,
    mutation_settings: crate::configuration::MutationSettings,
}

pub(crate) async fn run_owner(bootstrap: OwnerBootstrap) -> io::Result<()> {
    let settings = read_launch_settings(&bootstrap.launch_path)?;
    let _ = fs::remove_file(&bootstrap.launch_path);
    let owner_lock = acquire_owner_lock(&bootstrap.owner_lock_path)?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let started_at = now_rfc3339();
    let mut endpoint = OwnerEndpoint {
        format_version: 1,
        owner_protocol_version: OWNER_PROTOCOL_VERSION,
        session_identity: bootstrap.session_identity.clone(),
        owner_generation: bootstrap.owner_generation.clone(),
        token: bootstrap.token.clone(),
        address: listener.local_addr()?.to_string(),
        workspace_uri: bootstrap.workspace_uri.clone(),
        server: bootstrap.server.clone(),
        owner_pid: std::process::id(),
        started_at,
        state: "starting".to_owned(),
        failure: None,
    };
    write_endpoint(&bootstrap.endpoint_path, &endpoint)?;

    endpoint.state = "initializing".to_owned();
    write_endpoint(&bootstrap.endpoint_path, &endpoint)?;
    let mut lsp = match LspRuntime::start(&bootstrap, &settings).await {
        Ok(runtime) => runtime,
        Err(error) => {
            endpoint.state = "failed".to_owned();
            endpoint.failure = Some(error.to_string());
            let _ = write_endpoint(&bootstrap.endpoint_path, &endpoint);
            drop(owner_lock);
            return Err(error);
        }
    };
    endpoint.state = "ready".to_owned();
    write_endpoint(&bootstrap.endpoint_path, &endpoint)?;

    let (requests_tx, mut requests_rx) = mpsc::channel(OWNER_QUEUE_LIMIT);
    let listener_task = spawn_owner_listener(listener, endpoint.clone(), requests_tx);
    let started = Instant::now();
    let mut last_connection_closed = Instant::now();
    let mut idle_deadline =
        last_connection_closed + Duration::from_millis(settings.idle_timeout_ms);
    let mut should_stop = false;
    let mut active_queries = BTreeMap::new();

    while !should_stop {
        let idle_sleep = tokio_time::sleep_until(TokioInstant::from_std(idle_deadline));
        tokio::pin!(idle_sleep);
        tokio::select! {
            pending = requests_rx.recv() => {
                let Some(pending) = pending else { break };
                if *pending.cancelled.borrow() {
                    continue;
                }
                match pending.request {
                    OwnerRequest::Status => {
                        let response = OwnerResponse::success(
                            &bootstrap.owner_generation,
                            status_result(&bootstrap, &lsp, started, requests_rx.len(), &active_queries, idle_deadline),
                        );
                        let _ = pending.response.send(response);
                    }
                    OwnerRequest::Capabilities => {
                        let result = json!({
                            "protocolBaseline": "3.17",
                            "clientProfileVersion": CAPABILITY_PROFILE_VERSION,
                            "providers": lsp.negotiated.providers_json(),
                            "initializeResult": lsp.negotiated.initialize_result,
                            "positionEncoding": lsp.negotiated.position_encoding.name(),
                            "textSynchronization": match lsp.negotiated.text_synchronization {
                                crate::workspace::TextSynchronization::None => "none",
                                crate::workspace::TextSynchronization::OpenClose => "open_close",
                            }
                        });
                        let _ = pending.response.send(OwnerResponse::success(&bootstrap.owner_generation, result));
                    }
                    OwnerRequest::Logs { tail } => {
                        let result = lsp.log.render(&bootstrap.owner_generation, tail);
                        let _ = pending.response.send(OwnerResponse::success(&bootstrap.owner_generation, result));
                    }
                    OwnerRequest::Diagnostics { documents } => {
                        let response = match lsp.synchronize_documents(&documents).await {
                            Ok(()) => {
                                let versions = documents.iter().filter_map(|document| {
                                    let uri = Url::from_file_path(&document.path).ok()?.to_string();
                                    let version = lsp.documents.get(&uri)?.version;
                                    Some((uri, json!(version)))
                                }).collect::<serde_json::Map<_, _>>();
                                OwnerResponse::success(&bootstrap.owner_generation, json!({
                                    "state": lsp.diagnostics.export_state(),
                                    "documentVersions": versions
                                }))
                            }
                            Err(failure) => OwnerResponse::failure(
                                &bootstrap.owner_generation,
                                contract_failure_value(failure),
                            ),
                        };
                        let _ = pending.response.send(response);
                    }
                    OwnerRequest::Stop { force } => {
                        endpoint.state = "draining".to_owned();
                        let _ = write_endpoint(&bootstrap.endpoint_path, &endpoint);
                        fail_active_queries(
                            &bootstrap.owner_generation,
                            &mut active_queries,
                            request_cancelled("owner_stop"),
                        );
                        if force {
                            lsp.process.terminate_process_tree(Duration::ZERO).await;
                        } else {
                            lsp.graceful_shutdown().await;
                        }
                        let result = json!({
                            "ownerGeneration": bootstrap.owner_generation,
                            "outcome": if force { "force_stopped" } else { "stopped" },
                            "recoveryRequired": false
                        });
                        let _ = pending.response.send(OwnerResponse::success(&bootstrap.owner_generation, result));
                        let _ = tokio_time::timeout(Duration::from_secs(1), pending.delivered).await;
                        should_stop = true;
                    }
                    OwnerRequest::Dispatch { method, params, documents, request_timeout_ms, trace_protocol, apply_edits } => {
                        lsp.start_dispatch(
                            &bootstrap.owner_generation,
                            method,
                            params,
                            documents,
                            Duration::from_millis(request_timeout_ms),
                            trace_protocol,
                            apply_edits,
                            pending.response,
                            pending.cancelled,
                            &mut active_queries,
                        ).await;
                    }
                }
                last_connection_closed = Instant::now();
                idle_deadline = last_connection_closed + Duration::from_millis(settings.idle_timeout_ms);
            }
            stderr = lsp.stderr.recv() => {
                if let Some(stderr) = stderr {
                    lsp.log.push("server_stderr", "error", stderr);
                }
            }
            frame = lsp.reader.read_json_rpc_frame_with_bytes() => {
                match frame {
                    Ok(Some(frame)) => {
                        if let Err(error) = lsp.handle_concurrent_frame(
                            &bootstrap.owner_generation,
                            frame,
                            &mut active_queries,
                        ).await {
                            lsp.log.push("protocol_violation", "error", &error);
                            fail_active_queries(
                                &bootstrap.owner_generation,
                                &mut active_queries,
                                protocol_failure(error),
                            );
                            lsp.process.terminate_process_tree(Duration::ZERO).await;
                            should_stop = true;
                        }
                    }
                    Ok(None) => {
                        let failure = server_exited_failure(lsp.process.try_wait().ok().flatten());
                        fail_active_queries(&bootstrap.owner_generation, &mut active_queries, failure);
                        should_stop = true;
                    }
                    Err(error) => {
                        fail_active_queries(
                            &bootstrap.owner_generation,
                            &mut active_queries,
                            protocol_failure(error.to_string()),
                        );
                        lsp.process.terminate_process_tree(Duration::ZERO).await;
                        should_stop = true;
                    }
                }
            }
            _ = &mut idle_sleep, if active_queries.is_empty() => {
                lsp.log.push("lifecycle", "info", "Owner idle timeout reached");
                lsp.graceful_shutdown().await;
                should_stop = true;
            }
            _ = tokio_time::sleep(Duration::from_millis(25)) => {
                if lsp.maintain_active_queries(
                    &bootstrap.owner_generation,
                    &mut active_queries,
                ).await {
                    should_stop = true;
                }
                if let Some(status) = lsp.process.try_wait()? {
                    lsp.log.push("lifecycle", "error", format!("Language server exited: {status}"));
                    fail_active_queries(
                        &bootstrap.owner_generation,
                        &mut active_queries,
                        server_exited_failure(Some(status)),
                    );
                    should_stop = true;
                }
            }
        }
    }

    listener_task.abort();
    fail_active_queries(
        &bootstrap.owner_generation,
        &mut active_queries,
        request_cancelled("owner_stopped"),
    );
    let _ = fs::remove_file(&bootstrap.endpoint_path);
    drop(owner_lock);
    Ok(())
}

impl LspRuntime {
    async fn start(bootstrap: &OwnerBootstrap, settings: &OwnerLaunchSettings) -> io::Result<Self> {
        let (process, stdin, stdout, mut stderr) =
            SupervisedServerProcess::spawn(&bootstrap.executable, &settings.server_args)?;
        let (stderr_tx, stderr_rx) = mpsc::channel(64);
        tokio::spawn(async move {
            let mut bytes = vec![0; 8192];
            loop {
                match stderr.read(&mut bytes).await {
                    Ok(0) | Err(_) => break,
                    Ok(length) => {
                        let message = String::from_utf8_lossy(&bytes[..length]).into_owned();
                        if stderr_tx.send(message).await.is_err() {
                            break;
                        }
                    }
                }
            }
        });
        let body_limit = NonZeroUsize::new(settings.max_message_bytes)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "message limit is zero"))?;
        let root_name = bootstrap
            .workspace_path
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| bootstrap.workspace_uri.clone());
        let workspace_folder = json!({"uri": bootstrap.workspace_uri, "name": root_name});
        let placeholder = NegotiatedCapabilities {
            initialize_result: Value::Null,
            providers: BTreeMap::new(),
            position_encoding: crate::workspace::PositionEncoding::Utf16,
            text_synchronization: crate::workspace::TextSynchronization::None,
        };
        let mut runtime = Self {
            process,
            reader: JsonRpcFrameReader::with_body_limit(stdout, body_limit),
            writer: JsonRpcFrameWriter::with_body_limit(stdin, body_limit),
            stderr: stderr_rx,
            next_request_id: 1,
            negotiated: placeholder,
            diagnostics: DiagnosticCache::new(
                settings.max_diagnostic_snapshots,
                settings.max_diagnostic_bytes,
            ),
            documents: DocumentStore::new(
                settings.max_open_documents,
                settings.max_document_bytes,
                settings.max_total_text_bytes,
            ),
            progress: BTreeMap::new(),
            settings: settings.settings.clone(),
            workspace_uri: bootstrap.workspace_uri.clone(),
            workspace_path: bootstrap.workspace_path.clone(),
            server: bootstrap.server.clone(),
            session_identity: bootstrap.session_identity.clone(),
            workspace_folder,
            cancellation_grace: Duration::from_millis(settings.cancellation_grace_ms),
            shutdown_timeout: Duration::from_millis(settings.shutdown_timeout_ms),
            max_partial_result_bytes: settings.max_partial_result_bytes,
            log: SessionLog::new(),
            startup_trace: settings.trace_initialization.then(ProtocolTrace::new),
            preview_settings: settings.previews.clone(),
            receipt_settings: settings.receipts.clone(),
            mutation_settings: settings.mutation.clone(),
        };
        runtime
            .log
            .push("lifecycle", "info", "Language server process started");
        runtime
            .initialize(
                settings.initialization_options.clone(),
                Duration::from_millis(settings.initialization_timeout_ms),
            )
            .await?;
        runtime
            .log
            .push("lifecycle", "info", "Language server initialized");
        Ok(runtime)
    }

    async fn initialize(
        &mut self,
        initialization_options: Option<Value>,
        timeout: Duration,
    ) -> io::Result<()> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": 0,
            "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "clientInfo": {"name": "lspc", "version": env!("CARGO_PKG_VERSION")},
                "rootUri": self.workspace_uri,
                "capabilities": fixed_initialize_capabilities(),
                "initializationOptions": initialization_options,
                "workspaceFolders": [self.workspace_folder.clone()],
                "workDoneToken": "lspc-initialize-1"
            }
        });
        self.write_lsp_message(&request, true).await?;
        let deadline = TokioInstant::now() + timeout;
        let initialize_result = loop {
            tokio::select! {
                _ = tokio_time::sleep_until(deadline) => {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "LSP initialization timed out"));
                }
                stderr = self.stderr.recv() => {
                    if let Some(stderr) = stderr {
                        self.log.push("server_stderr", "error", stderr);
                    }
                }
                frame = self.reader.read_json_rpc_frame_with_bytes() => {
                    let frame = frame
                        .map_err(io::Error::other)?
                        .ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "Server exited during initialization"))?;
                    if let Some(trace) = &mut self.startup_trace {
                        trace.push("server_to_client", &frame.header, &frame.body, &frame.message);
                    }
                    if is_response_for(&frame.message, &json!(0)) {
                        if let Some(error) = frame.message.get("error") {
                            return Err(io::Error::other(format!("Initialize failed: {error}")));
                        }
                        break frame.message.get("result").cloned().ok_or_else(|| {
                            io::Error::new(io::ErrorKind::InvalidData, "Initialize response omitted result")
                        })?;
                    }
                    self.handle_initialization_message(frame.message).await?;
                }
            }
        };
        self.negotiated = normalize_initialize_result(initialize_result)
            .map_err(|reason| io::Error::new(io::ErrorKind::InvalidData, reason))?;
        self.write_lsp_message(
            &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
            true,
        )
        .await
    }

    async fn handle_initialization_message(&mut self, message: Value) -> io::Result<()> {
        if let (Some(id), Some(method)) = (message.get("id").cloned(), message["method"].as_str()) {
            let response = if method == "window/showMessageRequest" {
                json!({"jsonrpc": "2.0", "id": id, "result": null})
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32600,
                        "message": "Initialization in progress",
                        "data": {"reason": "initialization_in_progress"}
                    }
                })
            };
            return self.write_lsp_message(&response, true).await;
        }
        self.handle_notification(&message, None);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_dispatch(
        &mut self,
        owner_generation: &str,
        method: String,
        params: Option<Value>,
        documents: Vec<OwnerDocumentInput>,
        timeout: Duration,
        trace_protocol: bool,
        apply_edits: bool,
        response: oneshot::Sender<OwnerResponse>,
        cancelled: watch::Receiver<bool>,
        active: &mut BTreeMap<i64, ActiveQuery>,
    ) {
        if active.len() >= OWNER_QUEUE_LIMIT {
            let _ = response.send(OwnerResponse::failure(
                owner_generation,
                json!({
                    "category": "unavailable",
                    "code": "owner_queue_full",
                    "message": "The Owner has reached its active request limit.",
                    "stage": "queue",
                    "delivery": "not_sent",
                    "retry": "safe",
                    "data": {"limit": OWNER_QUEUE_LIMIT, "depth": active.len()}
                }),
            ));
            return;
        }
        if let Err(failure) = self.synchronize_documents(&documents).await {
            let _ = response.send(OwnerResponse::failure(
                owner_generation,
                contract_failure_value(failure),
            ));
            return;
        }
        let synchronization = self.synchronization_result(&method, &documents);
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let request_params = params.clone();
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            request["params"] = params;
        }
        let partial_token = request.pointer("/params/partialResultToken").cloned();
        let mut trace = trace_protocol.then(|| self.startup_trace.take().unwrap_or_default());
        if let Err(error) = self
            .write_lsp_message_traced(&request, trace.as_mut())
            .await
        {
            let _ = response.send(OwnerResponse::failure(
                owner_generation,
                transport_failure(error),
            ));
            return;
        }
        active.insert(
            id,
            ActiveQuery {
                method,
                params: request_params,
                started_at: now_rfc3339(),
                response: Some(response),
                cancelled,
                deadline: TokioInstant::now() + timeout,
                timeout,
                cancellation: None,
                partial_token,
                partial_items: Vec::new(),
                partial_bytes: 0,
                trace,
                documents,
                apply_edits,
                apply_edit_ledger: Vec::new(),
                synchronization,
            },
        );
    }

    async fn handle_concurrent_frame(
        &mut self,
        owner_generation: &str,
        frame: JsonRpcFrame,
        active: &mut BTreeMap<i64, ActiveQuery>,
    ) -> Result<(), String> {
        for query in active.values_mut() {
            if let Some(trace) = &mut query.trace {
                trace.push(
                    "server_to_client",
                    &frame.header,
                    &frame.body,
                    &frame.message,
                );
            }
        }
        let message = frame.message;
        if message.get("method").is_none() && message.get("id").is_some() {
            let id = message.get("id").and_then(Value::as_i64).ok_or_else(|| {
                "The server returned a response with a non-integer identifier".to_owned()
            })?;
            let Some(mut query) = active.remove(&id) else {
                return Err(
                    "The server returned an unknown or duplicate response identifier".to_owned(),
                );
            };
            let response = if let Some(cancellation) = query.cancellation.take() {
                let mut failure = cancellation.failure;
                attach_trace(&mut failure, query.trace);
                OwnerResponse::failure(owner_generation, failure)
            } else if let Some(error) = message.get("error") {
                let mut failure = server_error_failure(error);
                attach_trace(&mut failure, query.trace);
                OwnerResponse::failure(owner_generation, failure)
            } else if message.get("result").is_some() {
                let result = message.get("result").cloned().unwrap_or(Value::Null);
                self.record_pull_diagnostics(&query.method, query.params.as_ref(), &result);
                match self
                    .validate_documents_after_query(&query.documents, &result)
                    .await
                {
                    Ok(()) => {
                        let mut output = json!({
                            "result": result,
                            "partialResults": query.partial_items,
                            "applyEditLedger": query.apply_edit_ledger,
                            "synchronization": query.synchronization,
                            "positionEncoding": self.negotiated.position_encoding.name(),
                            "textSynchronization": match self.negotiated.text_synchronization {
                                crate::workspace::TextSynchronization::None => "none",
                                crate::workspace::TextSynchronization::OpenClose => "open_close",
                            }
                        });
                        if let Some(trace) = query.trace {
                            output["trace"] = trace.render();
                        }
                        OwnerResponse::success(owner_generation, output)
                    }
                    Err(failure) => {
                        OwnerResponse::failure(owner_generation, contract_failure_value(failure))
                    }
                }
            } else {
                return Err("A JSON-RPC response omitted both result and error".to_owned());
            };
            if let Some(sender) = query.response.take() {
                let _ = sender.send(response);
            }
            return Ok(());
        }

        if message.get("id").is_some() && message.get("method").is_some() {
            let response = if message["method"] == "workspace/applyEdit" {
                self.handle_apply_edit_callback(&message, active)
            } else {
                self.route_server_request(message).await
            };
            let (header, body) = self
                .writer
                .write_json_rpc_frame_with_bytes(&response)
                .await
                .map_err(|error| error.to_string())?;
            for query in active.values_mut() {
                if let Some(trace) = &mut query.trace {
                    trace.push("client_to_server", &header, &body, &response);
                }
            }
            return Ok(());
        }

        if message.get("method").is_some() && message.get("id").is_none() {
            let method = message["method"].as_str().unwrap_or_default();
            let token = message.pointer("/params/token");
            let mut matched_partial = false;
            if method == "$/progress" {
                for (id, query) in active.iter_mut() {
                    if token != query.partial_token.as_ref() {
                        continue;
                    }
                    matched_partial = true;
                    let value = message
                        .pointer("/params/value")
                        .cloned()
                        .unwrap_or(Value::Null);
                    query.partial_bytes = query.partial_bytes.saturating_add(
                        serde_json::to_vec(&value)
                            .map(|bytes| bytes.len())
                            .unwrap_or(usize::MAX),
                    );
                    match value {
                        Value::Array(items) => query.partial_items.extend(items),
                        value => query.partial_items.push(value),
                    }
                    if query.partial_bytes > self.max_partial_result_bytes
                        && query.cancellation.is_none()
                    {
                        let failure = json!({
                            "category": "query",
                            "code": "partial_result_too_large",
                            "message": "Partial result data exceeded the configured byte limit.",
                            "stage": "await_response",
                            "delivery": "uncertain",
                            "retry": "unsafe",
                            "data": {
                                "limit": self.max_partial_result_bytes,
                                "collectedBytes": query.partial_bytes,
                                "partialItemCount": query.partial_items.len()
                            },
                            "partialResult": {"items": query.partial_items.clone(), "complete": false}
                        });
                        query.cancellation = Some(QueryCancellation {
                            deadline: TokioInstant::now() + self.cancellation_grace,
                            failure,
                        });
                        let cancel = json!({
                            "jsonrpc": "2.0",
                            "method": "$/cancelRequest",
                            "params": {"id": id}
                        });
                        self.write_lsp_message(&cancel, false)
                            .await
                            .map_err(|error| error.to_string())?;
                    }
                }
            }
            if !matched_partial {
                self.handle_notification(&message, None);
            }
            return Ok(());
        }
        Err("Malformed JSON-RPC routing".to_owned())
    }

    fn handle_apply_edit_callback(
        &self,
        message: &Value,
        active: &mut BTreeMap<i64, ActiveQuery>,
    ) -> Value {
        let callback_id = message.get("id").cloned().unwrap_or(Value::Null);
        let candidates = active
            .iter()
            .filter(|(_, query)| query.method == "workspace/executeCommand")
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if candidates.len() != 1 {
            return json!({
                "jsonrpc": "2.0",
                "id": callback_id,
                "result": {"applied": false, "failureReason": "no_unique_preauthorized_request"}
            });
        }
        let query = active.get_mut(&candidates[0]).unwrap();
        let ordinal = query.apply_edit_ledger.len() as u64;
        let label = message.pointer("/params/label").and_then(Value::as_str);
        if !query.apply_edits {
            query.apply_edit_ledger.push(json!({
                "ordinal": ordinal,
                "label": label,
                "applied": false,
                "outcome": "rejected",
                "failureReason": "preview_required"
            }));
            return json!({
                "jsonrpc": "2.0",
                "id": callback_id,
                "result": {"applied": false, "failureReason": "preview_required"}
            });
        }
        if query.apply_edit_ledger.len() as u64
            >= self.mutation_settings.max_preauthorized_callbacks
        {
            query.apply_edit_ledger.push(json!({
                "ordinal": ordinal,
                "label": label,
                "applied": false,
                "outcome": "rejected",
                "failureReason": "callback_limit_exceeded"
            }));
            return json!({
                "jsonrpc": "2.0",
                "id": callback_id,
                "result": {"applied": false, "failureReason": "callback_limit_exceeded"}
            });
        }
        let Some(edit) = message.pointer("/params/edit").cloned() else {
            query.apply_edit_ledger.push(json!({
                "ordinal": ordinal,
                "label": label,
                "applied": false,
                "outcome": "rejected",
                "failureReason": "invalid_workspace_edit"
            }));
            return json_rpc_error(
                callback_id,
                -32602,
                "workspace/applyEdit requires params.edit",
                None,
            );
        };
        match crate::mutation::apply_preauthorized_workspace_edit(
            &self.workspace_path,
            &self.workspace_uri,
            &self.server,
            &self.session_identity,
            self.negotiated.position_encoding,
            label,
            edit,
            &self.preview_settings,
            &self.receipt_settings,
            &self.mutation_settings,
        ) {
            Ok(application) => {
                let result = &application["result"];
                let mut ledger = json!({
                    "ordinal": ordinal,
                    "label": label,
                    "applied": true,
                    "outcome": "applied",
                    "previewId": result.get("previewId"),
                    "receiptId": result.get("receiptId"),
                    "filesystemState": result.get("filesystemState")
                });
                compact_json_object(&mut ledger);
                query.apply_edit_ledger.push(ledger);
                json!({"jsonrpc": "2.0", "id": callback_id, "result": {"applied": true}})
            }
            Err(failure) => {
                let outcome = match failure.code {
                    "proposal_stale" | "preview_stale" => "stale",
                    "workspace_lock_timeout" => "busy",
                    "rolled_back" => "rolled_back",
                    "recovery_required" | "recovery_evidence_invalid" => "recovery_required",
                    _ => "rejected",
                };
                let mut ledger = json!({
                    "ordinal": ordinal,
                    "label": label,
                    "applied": false,
                    "outcome": outcome,
                    "failureReason": failure.code,
                    "failedChange": failure.data.get("failedChange")
                });
                compact_json_object(&mut ledger);
                query.apply_edit_ledger.push(ledger);
                json!({
                    "jsonrpc": "2.0",
                    "id": callback_id,
                    "result": {"applied": false, "failureReason": failure.code}
                })
            }
        }
    }

    async fn maintain_active_queries(
        &mut self,
        owner_generation: &str,
        active: &mut BTreeMap<i64, ActiveQuery>,
    ) -> bool {
        let now = TokioInstant::now();
        let mut cancel = Vec::new();
        let mut cancellation_grace_expired = false;
        for (id, query) in active.iter_mut() {
            if let Some(cancellation) = &query.cancellation {
                cancellation_grace_expired |= now >= cancellation.deadline;
                continue;
            }
            let caller_cancelled = *query.cancelled.borrow();
            if caller_cancelled || now >= query.deadline {
                let failure = if caller_cancelled {
                    request_cancelled("caller_disconnected")
                } else {
                    let mut failure = json!({
                        "category": "query",
                        "code": "request_timeout",
                        "message": "The language-server request timed out.",
                        "stage": "await_response",
                        "delivery": "uncertain",
                        "retry": "unsafe",
                        "data": {"timeout": format_duration(query.timeout)}
                    });
                    attach_trace(&mut failure, query.trace.take());
                    failure
                };
                query.cancellation = Some(QueryCancellation {
                    deadline: now + self.cancellation_grace,
                    failure,
                });
                cancel.push(*id);
            }
        }
        for id in cancel {
            if self
                .write_lsp_message(
                    &json!({
                        "jsonrpc": "2.0",
                        "method": "$/cancelRequest",
                        "params": {"id": id}
                    }),
                    false,
                )
                .await
                .is_err()
            {
                cancellation_grace_expired = true;
            }
        }
        if cancellation_grace_expired {
            self.process.terminate_process_tree(Duration::ZERO).await;
            fail_active_queries(
                owner_generation,
                active,
                protocol_failure(
                    "The server did not settle every cancelled request within the cancellation grace period"
                        .to_owned(),
                ),
            );
        }
        cancellation_grace_expired
    }

    async fn synchronize_documents(
        &mut self,
        documents: &[OwnerDocumentInput],
    ) -> Result<(), ContractFailure> {
        for document in documents {
            let outcome = self.documents.refresh(
                &document.path,
                &document.language_id,
                self.negotiated.text_synchronization,
            )?;
            if outcome.snapshot.digest != document.expected_digest {
                return Err(ContractFailure {
                    exit_code: 5,
                    category: "query",
                    code: "document_changed_while_reading",
                    message: "A synchronized Document changed before dispatch.".to_owned(),
                    stage: "synchronize",
                    delivery: "not_sent",
                    retry: "safe",
                    data: json!({
                        "uri": outcome.snapshot.uri,
                        "before": {"digest": document.expected_digest},
                        "after": {"digest": outcome.snapshot.digest}
                    }),
                });
            }
            self.send_synchronization_events(outcome.events).await?;
        }
        Ok(())
    }

    fn synchronization_result(&self, method: &str, documents: &[OwnerDocumentInput]) -> Value {
        let before = documents
            .iter()
            .filter_map(|document| {
                let uri = Url::from_file_path(&document.path).ok()?.to_string();
                let snapshot = self.documents.get(&uri)?;
                Some(json!({
                    "uri": snapshot.uri,
                    "digest": snapshot.digest,
                    "version": snapshot.version,
                    "languageId": snapshot.language_id
                }))
            })
            .collect::<Vec<_>>();
        let mode = if method == "workspace/diagnostic" {
            "workspace"
        } else if documents.is_empty() {
            "none"
        } else if documents.len() == 1 {
            "document"
        } else {
            "explicit"
        };
        json!({
            "mode": mode,
            "bestEffort": false,
            "before": before,
            "failures": [],
            "postResponseChanged": []
        })
    }

    async fn validate_documents_after_query(
        &mut self,
        documents: &[OwnerDocumentInput],
        server_result: &Value,
    ) -> Result<(), ContractFailure> {
        for document in documents {
            let outcome = self.documents.refresh(
                &document.path,
                &document.language_id,
                self.negotiated.text_synchronization,
            )?;
            if outcome.snapshot.digest != document.expected_digest {
                self.send_synchronization_events(outcome.events).await?;
                return Err(ContractFailure {
                    exit_code: 5,
                    category: "query",
                    code: "document_changed_during_query",
                    message:
                        "A synchronized Document changed while the server request was running."
                            .to_owned(),
                    stage: "validate_result",
                    delivery: "sent",
                    retry: "after_change",
                    data: json!({
                        "uri": outcome.snapshot.uri,
                        "beforeDigest": document.expected_digest,
                        "afterDigest": outcome.snapshot.digest,
                        "serverResult": server_result
                    }),
                });
            }
        }
        Ok(())
    }

    async fn send_synchronization_events(
        &mut self,
        events: Vec<SynchronizationEvent>,
    ) -> Result<(), ContractFailure> {
        for event in events {
            let notification = match event {
                SynchronizationEvent::DidOpen(snapshot) => json!({
                    "jsonrpc": "2.0",
                    "method": "textDocument/didOpen",
                    "params": {"textDocument": {
                        "uri": snapshot.uri,
                        "languageId": snapshot.language_id,
                        "version": snapshot.version,
                        "text": snapshot.text
                    }}
                }),
                SynchronizationEvent::DidClose { uri } => {
                    self.diagnostics.mark_closed(&uri);
                    json!({
                        "jsonrpc": "2.0",
                        "method": "textDocument/didClose",
                        "params": {"textDocument": {"uri": uri}}
                    })
                }
            };
            self.write_lsp_message(&notification, false)
                .await
                .map_err(|error| ContractFailure {
                    exit_code: 4,
                    category: "unavailable",
                    code: "owner_unavailable",
                    message: "Document synchronization could not be delivered.".to_owned(),
                    stage: "synchronize",
                    delivery: "uncertain",
                    retry: "unsafe",
                    data: json!({"reason": error.to_string()}),
                })?;
        }
        Ok(())
    }

    fn record_pull_diagnostics(&mut self, method: &str, params: Option<&Value>, result: &Value) {
        match method {
            "textDocument/diagnostic" => {
                if let Some(uri) = params
                    .and_then(|params| params.pointer("/textDocument/uri"))
                    .and_then(Value::as_str)
                {
                    self.diagnostics.apply_pull_report(uri, result.clone());
                }
            }
            "workspace/diagnostic" => {
                self.diagnostics.apply_workspace_pull_report(result.clone());
            }
            _ => {}
        }
    }

    async fn handle_server_message(
        &mut self,
        message: Value,
        partial_token: Option<&Value>,
        partial_items: &mut Vec<Value>,
        partial_bytes: &mut usize,
        trace: Option<&mut ProtocolTrace>,
    ) -> io::Result<()> {
        if message.get("id").is_some() && message.get("method").is_some() {
            let response = self.route_server_request(message).await;
            return self.write_lsp_message_traced(&response, trace).await;
        }
        if message.get("method").is_some() && message.get("id").is_none() {
            self.handle_notification(&message, partial_token);
            if partial_token.is_some()
                && message["method"] == "$/progress"
                && message.pointer("/params/token") == partial_token
            {
                let value = message
                    .pointer("/params/value")
                    .cloned()
                    .unwrap_or(Value::Null);
                *partial_bytes = partial_bytes.saturating_add(
                    serde_json::to_vec(&value)
                        .map(|bytes| bytes.len())
                        .unwrap_or(usize::MAX),
                );
                match value {
                    Value::Array(items) => partial_items.extend(items),
                    value => partial_items.push(value),
                }
            }
            return Ok(());
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Malformed JSON-RPC routing",
        ))
    }

    async fn route_server_request(&mut self, message: Value) -> Value {
        let id = message.get("id").cloned().unwrap_or(Value::Null);
        let method = message["method"].as_str().unwrap_or_default();
        if method == "window/workDoneProgress/create" {
            let token = message.pointer("/params/token").cloned();
            let key = token.as_ref().map(token_key);
            let invalid = key
                .as_ref()
                .is_none_or(|key| self.progress.contains_key(key))
                || self.progress.len() >= 256;
            return if invalid {
                json_rpc_error(id, -32602, "Invalid work-done progress token", None)
            } else {
                self.progress.insert(
                    key.unwrap(),
                    json!({"token": token.unwrap(), "kind": "work_done"}),
                );
                json!({"jsonrpc": "2.0", "id": id, "result": null})
            };
        }
        if method == "workspace/diagnostic/refresh" {
            self.diagnostics.invalidate_pull_result_ids();
        }
        let context = CallbackContext {
            settings: self.settings.clone(),
            workspace_uri: self.workspace_uri.clone(),
            workspace_path: self.workspace_path.clone(),
            workspace_folder: self.workspace_folder.clone(),
        };
        let mut router = Router::new(context);
        router.unhandled_request(|state, request| {
            let result = route_callback(state, &request.method, request.params);
            async move { result }
        });
        let request = match serde_json::from_value::<AnyRequest>(message) {
            Ok(request) => request,
            Err(error) => {
                return json_rpc_error(
                    id,
                    -32600,
                    "Invalid server request",
                    Some(json!({"reason": error.to_string()})),
                );
            }
        };
        match router.call(request).await {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({"jsonrpc": "2.0", "id": id, "error": error}),
        }
    }

    fn handle_notification(&mut self, message: &Value, partial_token: Option<&Value>) {
        let method = message["method"].as_str().unwrap_or_default();
        match method {
            "textDocument/publishDiagnostics" => {
                let uri = message.pointer("/params/uri").and_then(Value::as_str);
                let diagnostics = message.pointer("/params/diagnostics").cloned();
                let version = message.pointer("/params/version").and_then(Value::as_i64);
                if let (Some(uri), Some(diagnostics)) = (uri, diagnostics) {
                    self.diagnostics
                        .publish(uri, version, diagnostics, version, true);
                }
            }
            "window/logMessage" => {
                let text = message
                    .pointer("/params/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.log.push(
                    "window_log_message",
                    lsp_log_level(message.pointer("/params/type")),
                    text,
                );
            }
            "window/showMessage" => {
                let text = message
                    .pointer("/params/message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                self.log.push(
                    "window_show_message",
                    lsp_log_level(message.pointer("/params/type")),
                    text,
                );
            }
            "$/progress" if message.pointer("/params/token") != partial_token => {
                self.update_work_done_progress(message);
            }
            "telemetry/event" | "$/cancelRequest" => {}
            _ => {}
        }
    }

    fn update_work_done_progress(&mut self, message: &Value) {
        let Some(token) = message.pointer("/params/token").cloned() else {
            return;
        };
        let key = token_key(&token);
        let kind = message
            .pointer("/params/value/kind")
            .and_then(Value::as_str);
        if kind == Some("end") {
            self.progress.remove(&key);
            return;
        }
        if !self.progress.contains_key(&key) {
            return;
        }
        let value = message
            .pointer("/params/value")
            .cloned()
            .unwrap_or(Value::Null);
        let mut record = json!({"token": token, "kind": "work_done"});
        for field in ["title", "message", "percentage", "cancellable"] {
            if let Some(value) = value.get(field) {
                record[field] = value.clone();
            }
        }
        self.progress.insert(key, record);
    }

    async fn graceful_shutdown(&mut self) {
        if self.process.try_wait().ok().flatten().is_some() {
            return;
        }
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        if self
            .write_lsp_message(
                &json!({"jsonrpc": "2.0", "id": id, "method": "shutdown"}),
                false,
            )
            .await
            .is_ok()
        {
            let deadline = TokioInstant::now() + self.shutdown_timeout;
            loop {
                tokio::select! {
                    _ = tokio_time::sleep_until(deadline) => break,
                    frame = self.reader.read_json_rpc_frame_with_bytes() => {
                        match frame {
                            Ok(Some(frame)) if is_response_for(&frame.message, &json!(id)) => {
                                if frame.message.get("result") != Some(&Value::Null) {
                                    self.log.push("protocol_violation", "warning", "Shutdown returned a non-null result");
                                }
                                break;
                            }
                            Ok(Some(frame)) => {
                                let _ = self.handle_server_message(frame.message, None, &mut Vec::new(), &mut 0, None).await;
                            }
                            _ => break,
                        }
                    }
                }
            }
            let _ = self
                .write_lsp_message(&json!({"jsonrpc": "2.0", "method": "exit"}), false)
                .await;
        }
        if tokio_time::timeout(self.shutdown_timeout, self.process.wait())
            .await
            .is_err()
        {
            self.process.terminate_process_tree(Duration::ZERO).await;
        }
    }

    async fn write_lsp_message(&mut self, message: &Value, startup: bool) -> io::Result<()> {
        let bytes = self
            .writer
            .write_json_rpc_frame_with_bytes(message)
            .await
            .map_err(io::Error::other)?;
        if startup && let Some(trace) = &mut self.startup_trace {
            trace.push("client_to_server", &bytes.0, &bytes.1, message);
        }
        Ok(())
    }

    async fn write_lsp_message_traced(
        &mut self,
        message: &Value,
        trace: Option<&mut ProtocolTrace>,
    ) -> io::Result<()> {
        let (header, body) = self
            .writer
            .write_json_rpc_frame_with_bytes(message)
            .await
            .map_err(io::Error::other)?;
        if let Some(trace) = trace {
            trace.push("client_to_server", &header, &body, message);
        }
        Ok(())
    }
}

#[derive(Clone)]
struct CallbackContext {
    settings: Value,
    workspace_uri: String,
    workspace_path: PathBuf,
    workspace_folder: Value,
}

fn route_callback(
    context: &mut CallbackContext,
    method: &str,
    params: Value,
) -> Result<Value, ResponseError> {
    match method {
        "workspace/configuration" => Ok(workspace_configuration_result(context, &params)),
        "workspace/workspaceFolders" => Ok(json!([context.workspace_folder])),
        "workspace/diagnostic/refresh" => Ok(Value::Null),
        "workspace/applyEdit" => Ok(json!({
            "applied": false,
            "failureReason": "preview_required"
        })),
        "window/showMessageRequest" => Ok(Value::Null),
        "window/showDocument" => Ok(json!({"success": false})),
        _ => Err(ResponseError::new(
            ErrorCode::METHOD_NOT_FOUND,
            format!("Unsupported server request: {method}"),
        )),
    }
}

fn workspace_configuration_result(context: &CallbackContext, params: &Value) -> Value {
    let Some(items) = params.get("items").and_then(Value::as_array) else {
        return Value::Array(Vec::new());
    };
    Value::Array(
        items
            .iter()
            .map(|item| {
                if !configuration_scope_allowed(context, item.get("scopeUri")) {
                    return Value::Null;
                }
                let section = item
                    .get("section")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if section.is_empty() {
                    return context.settings.clone();
                }
                let mut value = &context.settings;
                for component in section.split('.') {
                    if component.is_empty() {
                        return Value::Null;
                    }
                    let Some(next) = value.as_object().and_then(|object| object.get(component))
                    else {
                        return Value::Null;
                    };
                    value = next;
                }
                value.clone()
            })
            .collect(),
    )
}

fn configuration_scope_allowed(context: &CallbackContext, scope_uri: Option<&Value>) -> bool {
    let Some(scope_uri) = scope_uri.and_then(Value::as_str) else {
        return true;
    };
    if scope_uri == context.workspace_uri {
        return true;
    }
    Url::parse(scope_uri)
        .ok()
        .filter(|uri| uri.scheme() == "file")
        .and_then(|uri| uri.to_file_path().ok())
        .and_then(|path| dunce::canonicalize(path).ok())
        .is_some_and(|path| path.starts_with(&context.workspace_path))
}

fn spawn_owner_listener(
    listener: TcpListener,
    endpoint: OwnerEndpoint,
    requests: mpsc::Sender<PendingOwnerRequest>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let endpoint = endpoint.clone();
            let requests = requests.clone();
            tokio::spawn(async move {
                let _ = handle_owner_connection(stream, endpoint, requests).await;
            });
        }
    })
}

async fn handle_owner_connection(
    mut stream: TcpStream,
    endpoint: OwnerEndpoint,
    requests: mpsc::Sender<PendingOwnerRequest>,
) -> io::Result<()> {
    let request: AuthenticatedOwnerRequest = read_owner_message(&mut stream).await?;
    if request.owner_protocol_version != OWNER_PROTOCOL_VERSION
        || request.session_identity != endpoint.session_identity
        || request.owner_generation != endpoint.owner_generation
        || !constant_time_token_matches(&request.token, &endpoint.token)
    {
        let failure = OwnerResponse::failure(
            &endpoint.owner_generation,
            json!({
                "category": "unavailable",
                "code": "owner_unavailable",
                "message": "Owner authentication or identity verification failed.",
                "stage": "discover_owner",
                "delivery": "not_sent",
                "retry": "safe",
                "data": {"sessionIdentity": endpoint.session_identity, "reason": "authentication_failed"}
            }),
        );
        write_owner_message(&mut stream, &failure).await?;
        return Ok(());
    }
    let (response_tx, response_rx) = oneshot::channel();
    let (delivered_tx, delivered_rx) = oneshot::channel();
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let pending = PendingOwnerRequest {
        request: request.request,
        response: response_tx,
        delivered: delivered_rx,
        cancelled: cancel_rx,
    };
    if requests.try_send(pending).is_err() {
        let failure = OwnerResponse::failure(
            &endpoint.owner_generation,
            json!({
                "category": "unavailable",
                "code": "owner_queue_full",
                "message": "The Owner operation queue is full.",
                "stage": "queue",
                "delivery": "not_sent",
                "retry": "safe",
                "data": {"limit": OWNER_QUEUE_LIMIT, "depth": OWNER_QUEUE_LIMIT}
            }),
        );
        write_owner_message(&mut stream, &failure).await?;
        return Ok(());
    }
    tokio::select! {
        response = response_rx => {
            if let Ok(response) = response {
                write_owner_message(&mut stream, &response).await?;
                let _ = delivered_tx.send(());
            }
        }
        disconnected = stream.read_u8() => {
            let _ = disconnected;
            let _ = cancel_tx.send(true);
        }
    }
    Ok(())
}

fn status_result(
    bootstrap: &OwnerBootstrap,
    lsp: &LspRuntime,
    started: Instant,
    queue_depth: usize,
    active_queries: &BTreeMap<i64, ActiveQuery>,
    idle_deadline: Instant,
) -> Value {
    let active_query = active_queries.values().next().map(|query| {
        json!({
            "command": ["raw"],
            "method": query.method,
            "startedAt": query.started_at
        })
    });
    json!({
        "sessionIdentity": bootstrap.session_identity,
        "ownerGeneration": bootstrap.owner_generation,
        "workspaceUri": bootstrap.workspace_uri,
        "server": bootstrap.server,
        "state": "ready",
        "serverPid": lsp.process.pid(),
        "uptimeMs": started.elapsed().as_millis() as u64,
        "idleDeadline": rfc3339_after(idle_deadline.saturating_duration_since(Instant::now())),
        "queueDepth": queue_depth.saturating_add(active_queries.len()).min(OWNER_QUEUE_LIMIT),
        "activeQuery": active_query,
        "capabilities": lsp.negotiated.providers_json(),
        "progress": lsp.progress.values().collect::<Vec<_>>()
    })
}

#[derive(Default)]
struct ProtocolTrace {
    frames: Vec<Value>,
    retained_bytes: usize,
    omitted_messages: u64,
    omitted_bytes: u64,
}

impl ProtocolTrace {
    fn new() -> Self {
        Self::default()
    }

    fn push(&mut self, direction: &str, header: &[u8], body: &[u8], message: &Value) {
        let size = header.len().saturating_add(body.len());
        if self.retained_bytes.saturating_add(size) > TRACE_CAPACITY_BYTES {
            self.omitted_messages = self.omitted_messages.saturating_add(1);
            self.omitted_bytes = self.omitted_bytes.saturating_add(size as u64);
            return;
        }
        let ordinal = self.frames.len();
        self.retained_bytes = self.retained_bytes.saturating_add(size);
        self.frames.push(json!({
            "ordinal": ordinal,
            "direction": direction,
            "headerHex": hex::encode(header),
            "bodyHex": hex::encode(body),
            "message": message
        }));
    }

    fn render(self) -> Value {
        json!({
            "retainedBytes": self.retained_bytes,
            "limitBytes": TRACE_CAPACITY_BYTES,
            "truncated": self.omitted_messages > 0,
            "omittedMessages": self.omitted_messages,
            "omittedBytes": self.omitted_bytes,
            "frames": self.frames
        })
    }
}

pub(crate) fn read_launch_settings(path: &Path) -> io::Result<OwnerLaunchSettings> {
    let bytes = fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn acquire_owner_lock(path: &Path) -> io::Result<std::fs::File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)?;
    file.try_lock_exclusive()?;
    Ok(file)
}

fn write_endpoint(path: &Path, endpoint: &OwnerEndpoint) -> io::Result<()> {
    let mut file = AtomicWriteFile::options().open(path)?;
    serde_json::to_writer(&mut file, endpoint).map_err(io::Error::other)?;
    file.commit()?;
    restrict_private_file(path)
}

fn restrict_private_file(path: &Path) -> io::Result<()> {
    crate::state_permissions::restrict_file(path)
}

fn is_response_for(message: &Value, id: &Value) -> bool {
    message.get("id") == Some(id)
        && message.get("method").is_none()
        && (message.get("result").is_some() ^ message.get("error").is_some())
}

fn token_key(token: &Value) -> String {
    serde_json::to_string(token).unwrap_or_default()
}

fn json_rpc_error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({"code": code, "message": message});
    if let Some(data) = data {
        error["data"] = data;
    }
    json!({"jsonrpc": "2.0", "id": id, "error": error})
}

fn compact_json_object(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.retain(|_, value| !value.is_null());
    }
}

fn lsp_log_level(value: Option<&Value>) -> &'static str {
    match value.and_then(Value::as_u64) {
        Some(1) => "error",
        Some(2) => "warning",
        Some(4) => "log",
        _ => "info",
    }
}

fn server_error_failure(error: &Value) -> Value {
    let code = error.get("code").and_then(Value::as_i64).unwrap_or(-32603);
    let classification = match code {
        -32800 => "request_cancelled",
        -32801 => "content_modified",
        -32802 => "server_cancelled",
        _ => "server_error",
    };
    let mut failure = json!({
        "category": "query",
        "code": classification,
        "message": error.get("message").and_then(Value::as_str).unwrap_or("The server returned an error."),
        "stage": "await_response",
        "delivery": "sent",
        "retry": "after_change",
        "data": {},
        "serverError": error
    });
    if classification == "server_cancelled" {
        failure["data"] = json!({
            "retriggerRequest": error.pointer("/data/retriggerRequest").and_then(Value::as_bool).unwrap_or(false)
        });
    } else if classification == "request_cancelled" {
        failure["data"] = json!({"source": "server"});
    }
    failure
}

fn contract_failure_value(failure: ContractFailure) -> Value {
    json!({
        "category": failure.category,
        "code": failure.code,
        "message": failure.message,
        "stage": failure.stage,
        "delivery": failure.delivery,
        "retry": failure.retry,
        "data": failure.data
    })
}

fn attach_trace(failure: &mut Value, trace: Option<ProtocolTrace>) {
    if let Some(trace) = trace {
        failure["trace"] = trace.render();
    }
}

fn fail_active_queries(
    owner_generation: &str,
    active: &mut BTreeMap<i64, ActiveQuery>,
    failure: Value,
) {
    for (_, mut query) in std::mem::take(active) {
        let mut failure = failure.clone();
        attach_trace(&mut failure, query.trace.take());
        if let Some(response) = query.response.take() {
            let _ = response.send(OwnerResponse::failure(owner_generation, failure));
        }
    }
}

fn transport_failure(error: io::Error) -> Value {
    json!({
        "category": "query", "code": "transport_failed", "message": "The language-server transport failed.",
        "stage": "await_response", "delivery": "uncertain", "retry": "unsafe",
        "data": {"reason": error.to_string(), "osCode": error.raw_os_error()}
    })
}

fn protocol_failure(reason: String) -> Value {
    json!({
        "category": "query", "code": "protocol_failed", "message": "The language-server protocol became unsafe.",
        "stage": "await_response", "delivery": "uncertain", "retry": "unsafe", "data": {"reason": reason}
    })
}

fn request_cancelled(source: &str) -> Value {
    json!({
        "category": "query", "code": "request_cancelled", "message": "The language-server request was cancelled.",
        "stage": "await_response", "delivery": "sent", "retry": "after_change", "data": {"source": source}
    })
}

fn server_exited_failure(status: Option<std::process::ExitStatus>) -> Value {
    json!({
        "category": "query", "code": "server_exited", "message": "The language server exited unexpectedly.",
        "stage": "await_response", "delivery": "uncertain", "retry": "unsafe",
        "data": {"status": process_status(status), "stderrTail": ""}
    })
}

fn process_status(status: Option<std::process::ExitStatus>) -> Value {
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(unix)]
    let signal = status.as_ref().and_then(ExitStatusExt::signal);
    #[cfg(not(unix))]
    let signal = Option::<i32>::None;
    json!({
        "success": status.as_ref().is_some_and(std::process::ExitStatus::success),
        "code": status.as_ref().and_then(std::process::ExitStatus::code),
        "signal": signal
    })
}

fn format_duration(duration: Duration) -> String {
    if duration.subsec_millis() == 0 {
        format!("{}s", duration.as_secs())
    } else {
        format!("{}ms", duration.as_millis())
    }
}

fn now_rfc3339() -> String {
    OffsetDateTime::now_utc().format(&Rfc3339).unwrap()
}

fn rfc3339_after(duration: Duration) -> String {
    (OffsetDateTime::now_utc()
        + ::time::Duration::try_from(duration).unwrap_or(::time::Duration::ZERO))
    .format(&Rfc3339)
    .unwrap()
}
