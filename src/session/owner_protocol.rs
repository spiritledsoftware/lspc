use std::{io, time::Duration};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub(crate) const OWNER_PROTOCOL_VERSION: u32 = 1;
pub(crate) const OWNER_QUEUE_LIMIT: usize = 64;
const OWNER_MESSAGE_LIMIT_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnerEndpoint {
    pub(crate) format_version: u32,
    pub(crate) owner_protocol_version: u32,
    pub(crate) session_identity: String,
    pub(crate) owner_generation: String,
    pub(crate) token: String,
    pub(crate) address: String,
    pub(crate) workspace_uri: String,
    pub(crate) server: String,
    pub(crate) owner_pid: u32,
    pub(crate) started_at: String,
    pub(crate) state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) failure: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AuthenticatedOwnerRequest {
    pub(crate) owner_protocol_version: u32,
    pub(crate) session_identity: String,
    pub(crate) owner_generation: String,
    pub(crate) token: String,
    pub(crate) request: OwnerRequest,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum OwnerRequest {
    Status,
    Capabilities,
    Logs {
        tail: usize,
    },
    Diagnostics {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        documents: Vec<OwnerDocumentInput>,
    },
    Stop {
        force: bool,
    },
    Dispatch {
        method: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        params: Option<Value>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        documents: Vec<OwnerDocumentInput>,
        request_timeout_ms: u64,
        trace_protocol: bool,
        apply_edits: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct OwnerDocumentInput {
    pub(crate) path: std::path::PathBuf,
    pub(crate) language_id: String,
    pub(crate) expected_digest: String,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnerResponse {
    pub(crate) owner_protocol_version: u32,
    pub(crate) owner_generation: String,
    pub(crate) ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) error: Option<Value>,
}

impl OwnerResponse {
    pub(crate) fn success(owner_generation: &str, result: Value) -> Self {
        Self {
            owner_protocol_version: OWNER_PROTOCOL_VERSION,
            owner_generation: owner_generation.to_owned(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub(crate) fn failure(owner_generation: &str, error: Value) -> Self {
        Self {
            owner_protocol_version: OWNER_PROTOCOL_VERSION,
            owner_generation: owner_generation.to_owned(),
            ok: false,
            result: None,
            error: Some(error),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OwnerLaunchSettings {
    pub(crate) session_identity: String,
    pub(crate) owner_generation: String,
    pub(crate) token: String,
    pub(crate) workspace_uri: String,
    pub(crate) server: String,
    pub(crate) server_args: Vec<String>,
    pub(crate) initialization_options: Option<Value>,
    pub(crate) settings: Value,
    pub(crate) initialization_timeout_ms: u64,
    pub(crate) cancellation_grace_ms: u64,
    pub(crate) shutdown_timeout_ms: u64,
    pub(crate) idle_timeout_ms: u64,
    pub(crate) max_message_bytes: usize,
    pub(crate) max_partial_result_bytes: usize,
    pub(crate) max_diagnostic_snapshots: u64,
    pub(crate) max_diagnostic_bytes: u64,
    pub(crate) max_open_documents: u64,
    pub(crate) max_document_bytes: u64,
    pub(crate) max_total_text_bytes: u64,
    pub(crate) previews: crate::configuration::PreviewSettings,
    pub(crate) receipts: crate::configuration::ReceiptSettings,
    pub(crate) mutation: crate::configuration::MutationSettings,
    pub(crate) trace_initialization: bool,
}

pub(crate) async fn read_owner_message<R, T>(input: &mut R) -> io::Result<T>
where
    R: AsyncRead + Unpin,
    T: for<'de> Deserialize<'de>,
{
    let length = input.read_u32().await? as usize;
    if length > OWNER_MESSAGE_LIMIT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Owner protocol message exceeds its byte limit",
        ));
    }
    let mut bytes = vec![0; length];
    input.read_exact(&mut bytes).await?;
    serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub(crate) async fn write_owner_message<W, T>(output: &mut W, value: &T) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
    T: Serialize,
{
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    if bytes.len() > OWNER_MESSAGE_LIMIT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Owner protocol message exceeds its byte limit",
        ));
    }
    output.write_u32(bytes.len() as u32).await?;
    output.write_all(&bytes).await?;
    output.flush().await
}

pub(crate) fn parse_duration(value: &str) -> Duration {
    if let Some(value) = value.strip_suffix("ms") {
        Duration::from_millis(value.parse().unwrap())
    } else if let Some(value) = value.strip_suffix('s') {
        Duration::from_secs(value.parse().unwrap())
    } else {
        Duration::from_secs(value.strip_suffix('m').unwrap().parse::<u64>().unwrap() * 60)
    }
}

pub(crate) fn constant_time_token_matches(left: &str, right: &str) -> bool {
    let different = left
        .as_bytes()
        .iter()
        .zip(right.as_bytes())
        .fold(left.len() ^ right.len(), |different, (left, right)| {
            different | usize::from(left ^ right)
        });
    different == 0
}
