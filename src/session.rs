#![allow(clippy::result_large_err)]

pub(crate) mod json_rpc_transport;

mod capabilities;
mod owner_protocol;
mod owner_runtime;
mod process_supervision;
mod session_log;

use std::{
    ffi::OsString,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
    thread,
    time::{Duration, Instant},
};

use atomic_write_file::AtomicWriteFile;
use fs2::FileExt;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use url::Url;

use crate::{
    cli::ParsedInvocation,
    configuration::{
        AuthorizedServer, LoadedConfiguration, authorize_server, load_configuration,
        select_configured_server, select_named_server, select_server,
    },
    contract::ContractFailure,
    query::{
        Capabilities, DispatchRequest, DispatchResponse, PreviewCreator, PreviewProposal, Provider,
        ProviderState, QueryContext, SessionDispatcher, compose, execute,
    },
    workspace::{
        DiagnosticCache, DocumentStore, PositionEncoding, TextSynchronization, select_workspace,
        validate_document_scope, workspace_from_path,
    },
};
use owner_protocol::{
    AuthenticatedOwnerRequest, OWNER_PROTOCOL_VERSION, OwnerEndpoint, OwnerLaunchSettings,
    OwnerRequest, OwnerResponse, parse_duration, read_owner_message, write_owner_message,
};
use owner_runtime::{OwnerBootstrap, read_launch_settings, run_owner};

/// Dispatches session lifecycle commands without starting an Owner during discovery.
pub(crate) fn dispatch_session_command(
    invocation: &ParsedInvocation,
) -> Result<Value, ContractFailure> {
    run_async(async {
        match invocation.command_path().get(1).map(String::as_str) {
            Some("list") => list_sessions(invocation).await,
            Some("status") => {
                let endpoint = select_live_endpoint(invocation).await?;
                let status = send_owner_request(&endpoint, OwnerRequest::Status).await?;
                Ok(success_envelope(invocation.command_path(), status))
            }
            Some("logs") => {
                let endpoint = select_live_endpoint(invocation).await?;
                let tail = invocation
                    .option_string("--tail")
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(100);
                let logs = send_owner_request(&endpoint, OwnerRequest::Logs { tail }).await?;
                Ok(success_envelope(invocation.command_path(), logs))
            }
            Some("stop") => {
                let endpoint = select_live_endpoint(invocation).await?;
                let stopped = send_owner_request(
                    &endpoint,
                    OwnerRequest::Stop {
                        force: invocation.has_option("--force"),
                    },
                )
                .await?;
                Ok(success_envelope(invocation.command_path(), stopped))
            }
            Some("restart") => restart_session(invocation).await,
            _ => unreachable!("the CLI catalog limits Session command paths"),
        }
    })
}

/// Runs every Query through the persistent Owner transport.
pub(crate) fn dispatch_owner_query_command(
    invocation: &ParsedInvocation,
) -> Result<Value, ContractFailure> {
    run_async(async {
        let current_directory = std::env::current_dir().map_err(|error| {
            owner_unavailable(
                "sid_0000000000000000000000000000000000000000000000000000000000000000",
                &error.to_string(),
            )
        })?;
        let mut targets = invocation.option_paths("--sync-file");
        if let Some(file) = invocation.option_path("--file") {
            targets.push(file);
        }
        let workspace = select_workspace(
            invocation.option_path("--workspace").as_deref(),
            &targets,
            &current_directory,
        )?;
        let configuration = load_configuration(
            &workspace.root,
            invocation.has_option("--ignore-project-config"),
        )?;
        let server = select_server(&configuration, invocation)?;
        let request_timeout = parse_duration(&server.request_timeout);
        let authorized = authorize_server(&configuration, server)?;
        let trace_protocol = invocation.has_option("--trace-protocol");
        let endpoint = connect_or_start_owner(&configuration, &authorized, trace_protocol).await?;
        let owner_capabilities = send_owner_request(&endpoint, OwnerRequest::Capabilities).await?;
        let capabilities = capabilities_from_owner(&owner_capabilities);
        let position_encoding = match owner_capabilities["positionEncoding"].as_str() {
            Some("utf-8") => PositionEncoding::Utf8,
            _ => PositionEncoding::Utf16,
        };
        let text_synchronization = match owner_capabilities["textSynchronization"].as_str() {
            Some("open_close") => TextSynchronization::OpenClose,
            _ => TextSynchronization::None,
        };

        let document = if let Some(path) = invocation.option_path("--file") {
            let path = validate_document_scope(
                &workspace,
                &path,
                invocation.has_option("--server"),
                false,
            )?;
            let language_id = crate::configuration::document_language_id(
                &configuration,
                &authorized.server.name,
                &path,
                invocation.option_string("--language-id").as_deref(),
            )?;
            let mut documents = DocumentStore::new(
                configuration.synchronization.max_open_documents,
                configuration.synchronization.max_document_bytes,
                configuration.synchronization.max_total_text_bytes,
            );
            Some(
                documents
                    .refresh(&path, &language_id, text_synchronization)?
                    .snapshot,
            )
        } else {
            None
        };
        let mut diagnostics = DiagnosticCache::new(
            configuration.synchronization.max_diagnostic_snapshots,
            configuration.synchronization.max_diagnostic_bytes,
        );
        let composed = compose(
            invocation,
            document.as_ref(),
            position_encoding,
            &capabilities,
            Some(&diagnostics),
        )?;
        let context = QueryContext {
            workspace_uri: configuration.workspace_uri.clone(),
            server: authorized.server.name.clone(),
            session_identity: authorized.session_identity.clone(),
            owner_generation: endpoint.owner_generation.clone(),
            result_position_encoding: position_encoding,
            synchronization: json!({
                "mode": match text_synchronization {
                    TextSynchronization::None => "none",
                    TextSynchronization::OpenClose => "open_close",
                },
                "bestEffort": false,
                "before": [],
                "failures": [],
                "postResponseChanged": []
            }),
            recovery: json!({"required": false}),
        };
        let mut dispatcher = OwnerQueryDispatcher {
            endpoint: &endpoint,
            request_timeout,
        };
        let mut previews = MutationPreviewCreator {
            configuration: &configuration,
            authorized: &authorized,
            position_encoding,
        };
        execute(
            &mut dispatcher,
            composed,
            context,
            &mut diagnostics,
            &mut previews,
        )
    })
}

fn capabilities_from_owner(value: &Value) -> Capabilities {
    let providers = value["providers"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, provider)| {
            let state = match provider["state"].as_str() {
                Some("supported") => ProviderState::Supported,
                Some("invalid") => ProviderState::Invalid,
                _ => ProviderState::Unsupported,
            };
            (
                name.clone(),
                Provider {
                    state,
                    capability_path: provider["capabilityPath"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    options: provider.get("options").cloned(),
                    selector: provider.get("documentSelector").cloned(),
                    problems: provider["problems"].as_array().cloned().unwrap_or_default(),
                },
            )
        })
        .collect();
    Capabilities {
        providers,
        initialize_result: value.get("initializeResult").cloned(),
    }
}

struct OwnerQueryDispatcher<'a> {
    endpoint: &'a OwnerEndpoint,
    request_timeout: Duration,
}

impl SessionDispatcher for OwnerQueryDispatcher<'_> {
    fn dispatch(
        &mut self,
        mut request: DispatchRequest,
    ) -> Result<DispatchResponse, ContractFailure> {
        if let Some(params) = request.params.as_mut() {
            if request.partial_results && params.is_object() {
                params["partialResultToken"] = json!(format!("lspc-partial-{}", random_hex(16)?));
            }
            if request.work_done_progress && params.is_object() {
                params["workDoneToken"] = json!(format!("lspc-work-{}", random_hex(16)?));
            }
        }
        let mut response = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(dispatch_owner_request(
                self.endpoint,
                request.method,
                request.params,
                self.request_timeout,
                request.trace_protocol,
            ))
        })?;
        let result = response
            .as_object_mut()
            .and_then(|object| object.remove("result"))
            .unwrap_or(Value::Null);
        let trace = response
            .as_object_mut()
            .and_then(|object| object.remove("trace"));
        let partial_results = response
            .as_object_mut()
            .and_then(|object| object.remove("partialResults"))
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        Ok(DispatchResponse {
            result,
            partial_results,
            trace,
            apply_edit_ledger: Vec::new(),
        })
    }
}

struct MutationPreviewCreator<'a> {
    configuration: &'a LoadedConfiguration,
    authorized: &'a AuthorizedServer,
    position_encoding: PositionEncoding,
}

impl PreviewCreator for MutationPreviewCreator<'_> {
    fn create_preview(&mut self, proposal: PreviewProposal) -> Result<Value, ContractFailure> {
        crate::mutation::create_query_preview(
            self.configuration,
            self.authorized,
            self.position_encoding,
            proposal,
        )
    }
}

/// Connects to an authenticated Owner or starts the sole lock-winning generation.
pub(crate) async fn connect_or_start_owner(
    configuration: &LoadedConfiguration,
    authorized: &AuthorizedServer,
    trace_initialization: bool,
) -> Result<OwnerEndpoint, ContractFailure> {
    let paths = OwnerStatePaths::new()?;
    if let Some(endpoint) = read_endpoint(&paths.endpoint_path(&authorized.session_identity))
        && probe_endpoint(&endpoint).await.is_ok()
    {
        return Ok(endpoint);
    }
    reauthorize_project_launch(configuration, authorized)?;
    let startup_timeout = parse_duration(&configuration.session.owner_startup_timeout);
    let startup_lock_path = paths.startup_lock_path(&authorized.session_identity);
    let startup_lock = acquire_startup_lock(&startup_lock_path, startup_timeout)?;
    if let Some(endpoint) = read_endpoint(&paths.endpoint_path(&authorized.session_identity))
        && probe_endpoint(&endpoint).await.is_ok()
    {
        drop(startup_lock);
        return Ok(endpoint);
    }
    let generation = format!("gen_{}", random_hex(16)?);
    let token = random_hex(32)?;
    let endpoint_path = paths.endpoint_path(&authorized.session_identity);
    let owner_lock_path = paths.owner_lock_path(&authorized.session_identity);
    let launch_path = paths.launch_path(&generation);
    let launch = OwnerLaunchSettings {
        session_identity: authorized.session_identity.clone(),
        owner_generation: generation,
        token,
        workspace_uri: configuration.workspace_uri.clone(),
        server: authorized.server.name.clone(),
        server_args: authorized.server.args.value.clone(),
        initialization_options: authorized
            .server
            .initialization_options
            .as_ref()
            .map(|value| value.value.clone()),
        settings: authorized.server.settings.value.clone(),
        initialization_timeout_ms: duration_millis(&configuration.session.initialization_timeout),
        cancellation_grace_ms: duration_millis(&configuration.session.cancellation_grace),
        shutdown_timeout_ms: duration_millis(&configuration.session.shutdown_timeout),
        idle_timeout_ms: duration_millis(&configuration.session.idle_timeout),
        max_message_bytes: configuration.protocol.max_message_bytes as usize,
        max_partial_result_bytes: configuration.protocol.max_partial_result_bytes as usize,
        max_diagnostic_snapshots: configuration.synchronization.max_diagnostic_snapshots,
        max_diagnostic_bytes: configuration.synchronization.max_diagnostic_bytes,
        trace_initialization,
    };
    write_private_json(&launch_path, &launch).map_err(|error| {
        owner_unavailable(
            &authorized.session_identity,
            &format!("launch_state_failed: {error}"),
        )
    })?;
    spawn_detached_owner(
        authorized,
        &configuration.workspace,
        &endpoint_path,
        &owner_lock_path,
        &launch_path,
    )?;
    let deadline = Instant::now() + startup_timeout;
    while Instant::now() < deadline {
        if let Some(endpoint) = read_endpoint(&endpoint_path) {
            if endpoint.state == "failed" {
                let reason = endpoint
                    .failure
                    .as_deref()
                    .unwrap_or("owner_initialization_failed")
                    .to_owned();
                let _ = fs::remove_file(&endpoint_path);
                drop(startup_lock);
                return Err(owner_unavailable(&authorized.session_identity, &reason));
            }
            if endpoint.session_identity == authorized.session_identity
                && probe_endpoint(&endpoint).await.is_ok()
            {
                drop(startup_lock);
                return Ok(endpoint);
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    drop(startup_lock);
    Err(owner_unavailable(
        &authorized.session_identity,
        "startup_deadline_exceeded",
    ))
}

fn reauthorize_project_launch(
    configuration: &LoadedConfiguration,
    authorized: &AuthorizedServer,
) -> Result<(), ContractFailure> {
    if authorized.declaration_digest.is_none() {
        return Ok(());
    }
    let fresh_configuration = load_configuration(&configuration.workspace, false)?;
    let fresh_server = select_configured_server(&fresh_configuration, &authorized.server.name)?;
    let fresh_authorized = authorize_server(&fresh_configuration, fresh_server)?;
    if fresh_authorized.declaration_digest != authorized.declaration_digest {
        return Err(ContractFailure {
            exit_code: 3,
            category: "blocked",
            code: "project_trust_changed",
            message:
                "Project-controlled language-server configuration changed before Owner startup."
                    .to_owned(),
            stage: "authorize",
            delivery: "not_sent",
            retry: "after_change",
            data: json!({
                "workspaceUri": configuration.workspace_uri,
                "server": authorized.server.name,
                "recordedDigest": authorized.declaration_digest,
                "currentDigest": fresh_authorized.declaration_digest,
                "changedFields": [],
                "requiredCommand": ["trust", "status", "--workspace", configuration.workspace, "--server", authorized.server.name]
            }),
        });
    }
    Ok(())
}

/// Sends one operation after authenticating endpoint identity and generation.
pub(crate) async fn dispatch_owner_request(
    endpoint: &OwnerEndpoint,
    method: String,
    params: Option<Value>,
    request_timeout: Duration,
    trace_protocol: bool,
) -> Result<Value, ContractFailure> {
    send_owner_request(
        endpoint,
        OwnerRequest::Dispatch {
            method,
            params,
            request_timeout_ms: request_timeout.as_millis() as u64,
            trace_protocol,
        },
    )
    .await
}

pub(crate) fn run_hidden_owner(arguments: &[OsString]) -> ExitCode {
    if arguments.len() != 7 {
        return ExitCode::from(1);
    }
    let workspace_path = PathBuf::from(&arguments[1]);
    let executable = PathBuf::from(&arguments[2]);
    let endpoint_path = PathBuf::from(&arguments[3]);
    let owner_lock_path = PathBuf::from(&arguments[4]);
    let launch_path = PathBuf::from(&arguments[5]);
    let settings = match read_launch_settings(&launch_path) {
        Ok(settings) => settings,
        Err(_) => return ExitCode::from(1),
    };
    let bootstrap = OwnerBootstrap {
        session_identity: settings.session_identity,
        owner_generation: settings.owner_generation,
        token: settings.token,
        workspace_uri: settings.workspace_uri,
        workspace_path,
        server: settings.server,
        executable,
        endpoint_path,
        owner_lock_path,
        launch_path,
    };
    match run_async(run_owner(bootstrap)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

async fn list_sessions(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let workspace_uri = invocation
        .option_path("--workspace")
        .map(|path| workspace_from_path(&path, true).map(|workspace| workspace.uri))
        .transpose()?;
    let server = invocation.option_string("--server");
    let mut sessions = Vec::new();
    for endpoint in live_endpoints().await? {
        if workspace_uri
            .as_ref()
            .is_some_and(|workspace| workspace != &endpoint.workspace_uri)
            || server
                .as_ref()
                .is_some_and(|server| server != &endpoint.server)
        {
            continue;
        }
        if let Ok(status) = send_owner_request(&endpoint, OwnerRequest::Status).await {
            sessions.push(status);
        }
    }
    sessions.sort_by(|left, right| {
        left["ownerGeneration"]
            .as_str()
            .cmp(&right["ownerGeneration"].as_str())
    });
    Ok(success_envelope(
        invocation.command_path(),
        Value::Array(sessions),
    ))
}

async fn restart_session(invocation: &ParsedInvocation) -> Result<Value, ContractFailure> {
    let endpoint = select_live_endpoint(invocation).await?;
    let previous_generation = endpoint.owner_generation.clone();
    let _ = send_owner_request(&endpoint, OwnerRequest::Stop { force: false }).await?;
    let workspace_path = Url::parse(&endpoint.workspace_uri)
        .ok()
        .and_then(|uri| uri.to_file_path().ok())
        .ok_or_else(|| session_selection_failure("The Owner Workspace URI is invalid.", vec![]))?;
    let configuration = load_configuration(&workspace_path, false)?;
    let server = select_named_server(&configuration, &endpoint.server, invocation)?;
    let authorized = authorize_server(&configuration, server)?;
    let replacement = connect_or_start_owner(&configuration, &authorized, false).await?;
    Ok(success_envelope(
        invocation.command_path(),
        json!({
            "previousOwnerGeneration": previous_generation,
            "ownerGeneration": replacement.owner_generation
        }),
    ))
}

async fn select_live_endpoint(
    invocation: &ParsedInvocation,
) -> Result<OwnerEndpoint, ContractFailure> {
    let endpoints = live_endpoints().await?;
    let selected = if let Some(generation) = invocation.positional_string(0) {
        endpoints
            .into_iter()
            .filter(|endpoint| endpoint.owner_generation == generation)
            .collect::<Vec<_>>()
    } else {
        let workspace_uri = invocation
            .option_path("--workspace")
            .map(|path| workspace_from_path(&path, true).map(|workspace| workspace.uri))
            .transpose()?;
        let server = invocation.option_string("--server");
        endpoints
            .into_iter()
            .filter(|endpoint| {
                workspace_uri
                    .as_ref()
                    .is_none_or(|workspace| workspace == &endpoint.workspace_uri)
                    && server
                        .as_ref()
                        .is_none_or(|server| server == &endpoint.server)
            })
            .collect::<Vec<_>>()
    };
    if selected.len() != 1 {
        return Err(session_selection_failure(
            if selected.is_empty() {
                "No live Owner matches the selector."
            } else {
                "The selector matches more than one live Owner."
            },
            selected
                .iter()
                .map(|endpoint| endpoint.owner_generation.clone())
                .collect(),
        ));
    }
    Ok(selected.into_iter().next().unwrap())
}

async fn live_endpoints() -> Result<Vec<OwnerEndpoint>, ContractFailure> {
    let paths = OwnerStatePaths::new()?;
    let entries = fs::read_dir(&paths.endpoints).map_err(|error| {
        owner_unavailable(
            "sid_0000000000000000000000000000000000000000000000000000000000000000",
            &error.to_string(),
        )
    })?;
    let mut endpoints = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(endpoint) = read_endpoint(&path) else {
            continue;
        };
        if probe_endpoint(&endpoint).await.is_ok() {
            endpoints.push(endpoint);
        } else if owner_lock_is_free(&paths.owner_lock_path(&endpoint.session_identity)) {
            let _ = fs::remove_file(path);
        }
    }
    Ok(endpoints)
}

async fn probe_endpoint(endpoint: &OwnerEndpoint) -> Result<(), ContractFailure> {
    let status = tokio::time::timeout(
        Duration::from_millis(500),
        send_owner_request(endpoint, OwnerRequest::Status),
    )
    .await
    .map_err(|_| owner_unavailable(&endpoint.session_identity, "probe_timed_out"))??;
    if status["ownerGeneration"] == endpoint.owner_generation {
        Ok(())
    } else {
        Err(owner_unavailable(
            &endpoint.session_identity,
            "generation_mismatch",
        ))
    }
}

async fn send_owner_request(
    endpoint: &OwnerEndpoint,
    request: OwnerRequest,
) -> Result<Value, ContractFailure> {
    if endpoint.owner_protocol_version != OWNER_PROTOCOL_VERSION {
        return Err(ContractFailure {
            exit_code: 4,
            category: "unavailable",
            code: "owner_protocol_incompatible",
            message: "The Owner protocol version is incompatible with this client.".to_owned(),
            stage: "discover_owner",
            delivery: "not_sent",
            retry: "after_change",
            data: json!({
                "clientVersion": OWNER_PROTOCOL_VERSION,
                "ownerVersion": endpoint.owner_protocol_version,
                "ownerGeneration": endpoint.owner_generation
            }),
        });
    }
    let mut stream = TcpStream::connect(&endpoint.address)
        .await
        .map_err(|error| owner_unavailable(&endpoint.session_identity, &error.to_string()))?;
    let authenticated = AuthenticatedOwnerRequest {
        owner_protocol_version: OWNER_PROTOCOL_VERSION,
        session_identity: endpoint.session_identity.clone(),
        owner_generation: endpoint.owner_generation.clone(),
        token: endpoint.token.clone(),
        request,
    };
    write_owner_message(&mut stream, &authenticated)
        .await
        .map_err(|error| owner_unavailable(&endpoint.session_identity, &error.to_string()))?;
    let response: OwnerResponse = read_owner_message(&mut stream)
        .await
        .map_err(|error| owner_unavailable(&endpoint.session_identity, &error.to_string()))?;
    if response.owner_protocol_version != OWNER_PROTOCOL_VERSION
        || response.owner_generation != endpoint.owner_generation
    {
        return Err(owner_unavailable(
            &endpoint.session_identity,
            "response_identity_mismatch",
        ));
    }
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        Err(contract_failure_from_owner(
            response.error.unwrap_or_else(|| json!({})),
        ))
    }
}

fn spawn_detached_owner(
    authorized: &AuthorizedServer,
    workspace: &Path,
    endpoint_path: &Path,
    owner_lock_path: &Path,
    launch_path: &Path,
) -> Result<(), ContractFailure> {
    let executable = std::env::current_exe().map_err(|error| {
        owner_unavailable(
            &authorized.session_identity,
            &format!("current_executable: {error}"),
        )
    })?;
    let mut command = Command::new(executable);
    command
        .arg("__owner")
        .arg(workspace)
        .arg(&authorized.executable)
        .arg(endpoint_path)
        .arg(owner_lock_path)
        .arg(launch_path)
        .arg("1")
        .current_dir(&authorized.cwd)
        .env_clear()
        .envs(authorized.child_environment.iter().cloned())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    detach_command(&mut command);
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| ContractFailure {
            exit_code: 5,
            category: "query",
            code: "server_start_failed",
            message: "The background Owner process could not start.".to_owned(),
            stage: "start_server",
            delivery: "not_sent",
            retry: "after_change",
            data: json!({
                "server": authorized.server.name,
                "executable": authorized.executable,
                "osCode": error.raw_os_error()
            }),
        })
}

#[cfg(unix)]
fn detach_command(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
}

#[cfg(windows)]
fn detach_command(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
}

#[cfg(not(any(unix, windows)))]
fn detach_command(_command: &mut Command) {}

struct OwnerStatePaths {
    endpoints: PathBuf,
    locks: PathBuf,
    launches: PathBuf,
}

impl OwnerStatePaths {
    fn new() -> Result<Self, ContractFailure> {
        let root = directories::ProjectDirs::from("", "", "lspc")
            .map(|directories| {
                directories
                    .state_dir()
                    .unwrap_or_else(|| directories.data_local_dir())
                    .to_path_buf()
            })
            .ok_or_else(|| {
                owner_unavailable(
                    "sid_0000000000000000000000000000000000000000000000000000000000000000",
                    "user_state_directory_unavailable",
                )
            })?
            .join("owners");
        let paths = Self {
            endpoints: root.join("endpoints"),
            locks: root.join("locks"),
            launches: root.join("launches"),
        };
        for path in [&paths.endpoints, &paths.locks, &paths.launches] {
            fs::create_dir_all(path).map_err(|error| {
                owner_unavailable(
                    "sid_0000000000000000000000000000000000000000000000000000000000000000",
                    &error.to_string(),
                )
            })?;
            restrict_private_directory(path).map_err(|error| {
                owner_unavailable(
                    "sid_0000000000000000000000000000000000000000000000000000000000000000",
                    &error.to_string(),
                )
            })?;
        }
        Ok(paths)
    }

    fn endpoint_path(&self, identity: &str) -> PathBuf {
        self.endpoints.join(format!("{identity}.json"))
    }
    fn owner_lock_path(&self, identity: &str) -> PathBuf {
        self.locks.join(format!("{identity}.lock"))
    }
    fn startup_lock_path(&self, identity: &str) -> PathBuf {
        self.locks.join(format!("{identity}.startup.lock"))
    }
    fn launch_path(&self, generation: &str) -> PathBuf {
        self.launches.join(format!("{generation}.json"))
    }
}

fn acquire_startup_lock(path: &Path, timeout: Duration) -> Result<std::fs::File, ContractFailure> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|error| {
            owner_unavailable(
                "sid_0000000000000000000000000000000000000000000000000000000000000000",
                &error.to_string(),
            )
        })?;
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) => {
                return Err(owner_unavailable(
                    "sid_0000000000000000000000000000000000000000000000000000000000000000",
                    &error.to_string(),
                ));
            }
        }
    }
}

fn owner_lock_is_free(path: &Path) -> bool {
    let Ok(file) = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
    else {
        return false;
    };
    file.try_lock_exclusive().is_ok()
}

fn write_private_json(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    let mut file = AtomicWriteFile::options().open(path)?;
    serde_json::to_writer(&mut file, value).map_err(io::Error::other)?;
    file.flush()?;
    file.commit()?;
    restrict_private_file(path)
}

fn read_endpoint(path: &Path) -> Option<OwnerEndpoint> {
    let bytes = fs::read(path).ok()?;
    let endpoint: OwnerEndpoint = serde_json::from_slice(&bytes).ok()?;
    (endpoint.format_version == 1).then_some(endpoint)
}

fn random_hex(bytes: usize) -> Result<String, ContractFailure> {
    let mut random = vec![0; bytes];
    getrandom::fill(&mut random).map_err(|error| {
        owner_unavailable(
            "sid_0000000000000000000000000000000000000000000000000000000000000000",
            &format!("secure_random_failed: {error}"),
        )
    })?;
    Ok(hex::encode(random))
}

fn duration_millis(value: &str) -> u64 {
    parse_duration(value).as_millis() as u64
}

fn success_envelope(command: &[String], result: Value) -> Value {
    json!({"schemaVersion": 1, "ok": true, "command": command, "result": result})
}

fn contract_failure_from_owner(error: Value) -> ContractFailure {
    let code = error
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("owner_unavailable");
    ContractFailure {
        exit_code: match error.get("category").and_then(Value::as_str) {
            Some("blocked") => 3,
            Some("unavailable") => 4,
            Some("query") => 5,
            _ => 70,
        },
        category: leak_contract_string(
            error
                .get("category")
                .and_then(Value::as_str)
                .unwrap_or("internal"),
        ),
        code: leak_contract_string(code),
        message: error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("The Owner request failed.")
            .to_owned(),
        stage: leak_contract_string(
            error
                .get("stage")
                .and_then(Value::as_str)
                .unwrap_or("discover_owner"),
        ),
        delivery: leak_contract_string(
            error
                .get("delivery")
                .and_then(Value::as_str)
                .unwrap_or("not_sent"),
        ),
        retry: leak_contract_string(error.get("retry").and_then(Value::as_str).unwrap_or("safe")),
        data: error.get("data").cloned().unwrap_or_else(|| json!({})),
    }
}

fn leak_contract_string(value: &str) -> &'static str {
    Box::leak(value.to_owned().into_boxed_str())
}

fn owner_unavailable(identity: &str, reason: &str) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "owner_unavailable",
        message: "The background Owner is unavailable.".to_owned(),
        stage: "discover_owner",
        delivery: "not_sent",
        retry: "safe",
        data: json!({"sessionIdentity": identity, "reason": reason}),
    }
}

fn session_selection_failure(reason: &str, candidates: Vec<String>) -> ContractFailure {
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "session_selection_failed",
        message: "The Session selector did not resolve one live Owner.".to_owned(),
        stage: "discover_owner",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"reason": reason, "candidates": candidates}),
    }
}

fn run_async<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("Tokio runtime construction is infallible on supported platforms")
        .block_on(future)
}

fn restrict_private_directory(path: &Path) -> io::Result<()> {
    crate::state_permissions::restrict_directory(path)
}

fn restrict_private_file(path: &Path) -> io::Result<()> {
    crate::state_permissions::restrict_file(path)
}
