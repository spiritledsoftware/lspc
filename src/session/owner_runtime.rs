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
    io::{AsyncReadExt, AsyncWriteExt},
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
    json_rpc_transport::{JsonRpcFrameReader, JsonRpcFrameWriter},
    owner_protocol::{
        AuthenticatedOwnerRequest, OWNER_PROTOCOL_VERSION, OWNER_QUEUE_LIMIT, OwnerEndpoint,
        OwnerLaunchSettings, OwnerRequest, OwnerResponse, constant_time_token_matches,
        read_owner_message, write_owner_message,
    },
    process_supervision::SupervisedServerProcess,
    session_log::SessionLog,
};
use crate::workspace::DiagnosticCache;

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
    cancelled: watch::Receiver<bool>,
}

struct LspRuntime {
    process: SupervisedServerProcess,
    reader: JsonRpcFrameReader<tokio::process::ChildStdout>,
    writer: JsonRpcFrameWriter<tokio::process::ChildStdin>,
    stderr: mpsc::Receiver<String>,
    next_request_id: i64,
    negotiated: NegotiatedCapabilities,
    diagnostics: DiagnosticCache,
    progress: BTreeMap<String, Value>,
    settings: Value,
    workspace_uri: String,
    workspace_path: PathBuf,
    workspace_folder: Value,
    cancellation_grace: Duration,
    shutdown_timeout: Duration,
    max_partial_result_bytes: usize,
    log: SessionLog,
    startup_trace: Option<ProtocolTrace>,
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

    while !should_stop {
        let idle_sleep = tokio_time::sleep_until(TokioInstant::from_std(idle_deadline));
        tokio::pin!(idle_sleep);
        tokio::select! {
            pending = requests_rx.recv() => {
                let Some(mut pending) = pending else { break };
                if *pending.cancelled.borrow() {
                    continue;
                }
                match pending.request {
                    OwnerRequest::Status => {
                        let response = OwnerResponse::success(
                            &bootstrap.owner_generation,
                            status_result(&bootstrap, &lsp, started, requests_rx.len(), idle_deadline),
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
                    OwnerRequest::Stop { force } => {
                        endpoint.state = "draining".to_owned();
                        let _ = write_endpoint(&bootstrap.endpoint_path, &endpoint);
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
                        should_stop = true;
                    }
                    OwnerRequest::Dispatch { method, params, request_timeout_ms, trace_protocol } => {
                        let response = lsp
                            .dispatch_request(
                                &bootstrap.owner_generation,
                                method,
                                params,
                                Duration::from_millis(request_timeout_ms),
                                trace_protocol,
                                &mut pending.cancelled,
                            )
                            .await;
                        let _ = pending.response.send(response);
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
            _ = &mut idle_sleep => {
                lsp.log.push("lifecycle", "info", "Owner idle timeout reached");
                lsp.graceful_shutdown().await;
                should_stop = true;
            }
            _ = tokio_time::sleep(Duration::from_millis(100)) => {
                if let Some(status) = lsp.process.try_wait()? {
                    lsp.log.push("lifecycle", "error", format!("Language server exited: {status}"));
                    should_stop = true;
                }
            }
        }
    }

    listener_task.abort();
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
            progress: BTreeMap::new(),
            settings: settings.settings.clone(),
            workspace_uri: bootstrap.workspace_uri.clone(),
            workspace_path: bootstrap.workspace_path.clone(),
            workspace_folder,
            cancellation_grace: Duration::from_millis(settings.cancellation_grace_ms),
            shutdown_timeout: Duration::from_millis(settings.shutdown_timeout_ms),
            max_partial_result_bytes: settings.max_partial_result_bytes,
            log: SessionLog::new(),
            startup_trace: settings.trace_initialization.then(ProtocolTrace::new),
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

    async fn dispatch_request(
        &mut self,
        owner_generation: &str,
        method: String,
        params: Option<Value>,
        timeout: Duration,
        trace_protocol: bool,
        cancelled: &mut watch::Receiver<bool>,
    ) -> OwnerResponse {
        let id = self.next_request_id;
        self.next_request_id = self.next_request_id.saturating_add(1);
        let mut request = json!({"jsonrpc": "2.0", "id": id, "method": method});
        if let Some(params) = params {
            request["params"] = params;
        }
        let partial_token = request.pointer("/params/partialResultToken").cloned();
        let mut partial_items = Vec::new();
        let mut partial_bytes = 0usize;
        let mut trace = trace_protocol.then(|| self.startup_trace.take().unwrap_or_default());
        if let Err(error) = self
            .write_lsp_message_traced(&request, trace.as_mut())
            .await
        {
            return OwnerResponse::failure(owner_generation, transport_failure(error));
        }
        let deadline = TokioInstant::now() + timeout;
        let response = loop {
            tokio::select! {
                _ = tokio_time::sleep_until(deadline) => {
                    let _ = self.write_lsp_message(
                        &json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}),
                        false,
                    ).await;
                    return self.finish_timed_out_request(owner_generation, id, timeout, trace).await;
                }
                changed = cancelled.changed() => {
                    if changed.is_ok() && *cancelled.borrow() {
                        let _ = self.write_lsp_message(
                            &json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}),
                            false,
                        ).await;
                        return OwnerResponse::failure(owner_generation, request_cancelled("caller_disconnected"));
                    }
                }
                stderr = self.stderr.recv() => {
                    if let Some(stderr) = stderr {
                        self.log.push("server_stderr", "error", stderr);
                    }
                }
                frame = self.reader.read_json_rpc_frame_with_bytes() => {
                    let frame = match frame {
                        Ok(Some(frame)) => frame,
                        Ok(None) => return OwnerResponse::failure(owner_generation, server_exited_failure(self.process.try_wait().ok().flatten())),
                        Err(error) => return OwnerResponse::failure(owner_generation, protocol_failure(error.to_string())),
                    };
                    if let Some(trace) = &mut trace {
                        trace.push("server_to_client", &frame.header, &frame.body, &frame.message);
                    }
                    if is_response_for(&frame.message, &json!(id)) {
                        break frame.message;
                    }
                    if frame.message.get("id").is_some() && frame.message.get("method").is_none() {
                        return OwnerResponse::failure(owner_generation, protocol_failure("Unknown or duplicate response identifier".to_owned()));
                    }
                    if let Err(error) = self.handle_server_message(frame.message, partial_token.as_ref(), &mut partial_items, &mut partial_bytes, trace.as_mut()).await {
                        return OwnerResponse::failure(owner_generation, protocol_failure(error.to_string()));
                    }
                    if partial_bytes > self.max_partial_result_bytes {
                        let _ = self.write_lsp_message(
                            &json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": id}}),
                            false,
                        ).await;
                        return OwnerResponse::failure(owner_generation, json!({
                            "category": "query",
                            "code": "partial_result_too_large",
                            "message": "Partial result data exceeded the configured byte limit.",
                            "stage": "await_response",
                            "delivery": "uncertain",
                            "retry": "unsafe",
                            "data": {
                                "limit": self.max_partial_result_bytes,
                                "collectedBytes": partial_bytes,
                                "partialItemCount": partial_items.len()
                            },
                            "partialResult": {"items": partial_items, "complete": false}
                        }));
                    }
                }
            }
        };
        let mut result = if let Some(error) = response.get("error") {
            return OwnerResponse::failure(owner_generation, server_error_failure(error));
        } else {
            response.get("result").cloned().unwrap_or(Value::Null)
        };
        if !partial_items.is_empty() {
            if let Some(items) = result.as_array_mut() {
                let mut combined = std::mem::take(&mut partial_items);
                combined.append(items);
                result = Value::Array(combined);
            }
        }
        let mut output = json!({
            "result": result,
            "partialResult": if partial_items.is_empty() { Value::Null } else { json!({"items": partial_items, "complete": true}) },
            "positionEncoding": self.negotiated.position_encoding.name(),
            "textSynchronization": match self.negotiated.text_synchronization {
                crate::workspace::TextSynchronization::None => "none",
                crate::workspace::TextSynchronization::OpenClose => "open_close",
            }
        });
        if let Some(trace) = trace {
            output["trace"] = trace.render();
        }
        OwnerResponse::success(owner_generation, output)
    }

    async fn finish_timed_out_request(
        &mut self,
        owner_generation: &str,
        id: i64,
        timeout: Duration,
        mut trace: Option<ProtocolTrace>,
    ) -> OwnerResponse {
        let deadline = TokioInstant::now() + self.cancellation_grace;
        loop {
            tokio::select! {
                _ = tokio_time::sleep_until(deadline) => {
                    self.process.terminate_process_tree(Duration::ZERO).await;
                    return OwnerResponse::failure(owner_generation, protocol_failure("The server did not terminate a cancelled request".to_owned()));
                }
                frame = self.reader.read_json_rpc_frame_with_bytes() => {
                    match frame {
                        Ok(Some(frame)) => {
                            if let Some(trace) = &mut trace {
                                trace.push("server_to_client", &frame.header, &frame.body, &frame.message);
                            }
                            if is_response_for(&frame.message, &json!(id)) {
                                let mut failure = json!({
                                    "category": "query",
                                    "code": "request_timeout",
                                    "message": "The language-server request timed out.",
                                    "stage": "await_response",
                                    "delivery": "uncertain",
                                    "retry": "unsafe",
                                    "data": {"timeout": format_duration(timeout)}
                                });
                                if let Some(trace) = trace {
                                    failure["trace"] = trace.render();
                                }
                                return OwnerResponse::failure(owner_generation, failure);
                            }
                            let _ = self.handle_server_message(frame.message, None, &mut Vec::new(), &mut 0, trace.as_mut()).await;
                        }
                        _ => return OwnerResponse::failure(owner_generation, protocol_failure("Transport failed during cancellation".to_owned())),
                    }
                }
            }
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
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let pending = PendingOwnerRequest {
        request: request.request,
        response: response_tx,
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
    idle_deadline: Instant,
) -> Value {
    json!({
        "sessionIdentity": bootstrap.session_identity,
        "ownerGeneration": bootstrap.owner_generation,
        "workspaceUri": bootstrap.workspace_uri,
        "server": bootstrap.server,
        "state": "ready",
        "serverPid": lsp.process.pid(),
        "uptimeMs": started.elapsed().as_millis() as u64,
        "idleDeadline": rfc3339_after(idle_deadline.saturating_duration_since(Instant::now())),
        "queueDepth": queue_depth.min(OWNER_QUEUE_LIMIT),
        "activeQuery": Value::Null,
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

#[cfg(unix)]
fn restrict_private_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn restrict_private_file(_path: &Path) -> io::Result<()> {
    Ok(())
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
