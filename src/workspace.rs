#![allow(clippy::result_large_err)]

mod diagnostics;

pub(crate) use diagnostics::{DiagnosticCache, DiagnosticResult};

use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata},
    io::{self, Read},
    path::{Path, PathBuf},
};

use serde::Serialize;
use serde_json::{Value, json};
use url::Url;

use crate::{canonical_value::digest_raw_bytes, contract::ContractFailure};

#[derive(Debug, Clone)]
pub(crate) struct Workspace {
    pub(crate) root: PathBuf,
    pub(crate) uri: String,
    pub(crate) explicitly_selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PositionEncoding {
    Utf8,
    Utf16,
}

impl PositionEncoding {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ProtocolPosition {
    pub(crate) line: u32,
    pub(crate) character: u32,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedPosition {
    pub(crate) byte_offset: usize,
    pub(crate) protocol: ProtocolPosition,
}

#[derive(Debug, Clone)]
pub(crate) struct DocumentSnapshot {
    pub(crate) path: PathBuf,
    pub(crate) uri: String,
    pub(crate) language_id: String,
    pub(crate) version: i64,
    pub(crate) text: String,
    pub(crate) digest: String,
    identity: FileIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FileIdentity {
    length: u64,
    modified_nanos: Option<u128>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
    #[cfg(windows)]
    volume_serial: Option<u32>,
    #[cfg(windows)]
    file_index: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TextSynchronization {
    None,
    OpenClose,
}

#[derive(Debug, Clone)]
pub(crate) enum SynchronizationEvent {
    DidOpen(DocumentSnapshot),
    DidClose { uri: String },
}

#[derive(Debug)]
pub(crate) struct RefreshOutcome {
    pub(crate) snapshot: DocumentSnapshot,
    pub(crate) events: Vec<SynchronizationEvent>,
    pub(crate) evicted_uris: Vec<String>,
}

#[derive(Debug)]
struct OpenDocument {
    snapshot: DocumentSnapshot,
    last_used: u64,
}

/// Bounded owner-local Document snapshots with explicit close/reopen refresh.
#[derive(Debug)]
pub(crate) struct DocumentStore {
    documents: BTreeMap<String, OpenDocument>,
    next_versions: BTreeMap<String, i64>,
    use_clock: u64,
    max_open_documents: usize,
    max_document_bytes: u64,
    max_total_text_bytes: u64,
    total_text_bytes: u64,
    pending_events: Vec<SynchronizationEvent>,
}

impl DocumentStore {
    pub(crate) fn new(
        max_open_documents: u64,
        max_document_bytes: u64,
        max_total_text_bytes: u64,
    ) -> Self {
        Self {
            documents: BTreeMap::new(),
            next_versions: BTreeMap::new(),
            use_clock: 0,
            max_open_documents: usize::try_from(max_open_documents).unwrap_or(usize::MAX),
            max_document_bytes,
            max_total_text_bytes,
            total_text_bytes: 0,
            pending_events: Vec::new(),
        }
    }

    pub(crate) fn refresh(
        &mut self,
        path: &Path,
        language_id: &str,
        synchronization: TextSynchronization,
    ) -> Result<RefreshOutcome, ContractFailure> {
        let (canonical, uri, text, digest, identity) =
            match read_document(path, self.max_document_bytes) {
                Ok(document) => document,
                Err(failure) => {
                    self.close_failed_document(path, synchronization);
                    return Err(failure);
                }
            };
        if self.max_open_documents == 0 || text.len() as u64 > self.max_total_text_bytes {
            self.close_failed_document(&canonical, synchronization);
            return Err(document_too_large(
                &canonical,
                &uri,
                text.len() as u64,
                self.max_total_text_bytes,
            ));
        }
        let existing = self.documents.remove(&uri);
        if let Some(existing) = &existing {
            self.total_text_bytes = self
                .total_text_bytes
                .saturating_sub(existing.snapshot.text.len() as u64);
        }
        let unchanged = existing.as_ref().is_some_and(|open| {
            open.snapshot.digest == digest && open.snapshot.language_id == language_id
        });
        let version = if unchanged {
            existing.as_ref().unwrap().snapshot.version
        } else {
            self.allocate_version(&uri)
        };
        let snapshot = DocumentSnapshot {
            path: canonical,
            uri: uri.clone(),
            language_id: language_id.to_owned(),
            version,
            text,
            digest,
            identity,
        };
        let mut events = Vec::new();
        if synchronization == TextSynchronization::OpenClose && !unchanged {
            if existing.is_some() {
                events.push(SynchronizationEvent::DidClose { uri: uri.clone() });
            }
            events.push(SynchronizationEvent::DidOpen(snapshot.clone()));
        }

        self.use_clock = self.use_clock.saturating_add(1);
        self.total_text_bytes = self
            .total_text_bytes
            .saturating_add(snapshot.text.len() as u64);
        self.documents.insert(
            uri.clone(),
            OpenDocument {
                snapshot: snapshot.clone(),
                last_used: self.use_clock,
            },
        );
        let (mut eviction_events, evicted_uris) = self.evict_to_limits(&uri, synchronization)?;
        events.append(&mut eviction_events);
        Ok(RefreshOutcome {
            snapshot,
            events,
            evicted_uris,
        })
    }

    pub(crate) fn refresh_open_documents(
        &mut self,
        synchronization: TextSynchronization,
    ) -> (Vec<RefreshOutcome>, Vec<ContractFailure>) {
        let inputs = self
            .documents
            .values()
            .map(|open| {
                (
                    open.snapshot.path.clone(),
                    open.snapshot.language_id.clone(),
                )
            })
            .collect::<Vec<_>>();
        let mut refreshed = Vec::new();
        let mut failures = Vec::new();
        for (path, language_id) in inputs {
            match self.refresh(&path, &language_id, synchronization) {
                Ok(outcome) => refreshed.push(outcome),
                Err(failure) => failures.push(failure),
            }
        }
        (refreshed, failures)
    }

    pub(crate) fn get(&self, uri: &str) -> Option<&DocumentSnapshot> {
        self.documents.get(uri).map(|open| &open.snapshot)
    }

    pub(crate) fn snapshots(&self) -> Vec<&DocumentSnapshot> {
        self.documents.values().map(|open| &open.snapshot).collect()
    }

    pub(crate) fn close_all(
        &mut self,
        synchronization: TextSynchronization,
    ) -> Vec<SynchronizationEvent> {
        let uris = self.documents.keys().cloned().collect::<Vec<_>>();
        self.documents.clear();
        self.total_text_bytes = 0;
        if synchronization == TextSynchronization::OpenClose {
            uris.into_iter()
                .map(|uri| SynchronizationEvent::DidClose { uri })
                .collect()
        } else {
            Vec::new()
        }
    }

    pub(crate) fn drain_pending_events(&mut self) -> Vec<SynchronizationEvent> {
        std::mem::take(&mut self.pending_events)
    }

    fn remove(&mut self, uri: &str) {
        if let Some(open) = self.documents.remove(uri) {
            self.total_text_bytes = self
                .total_text_bytes
                .saturating_sub(open.snapshot.text.len() as u64);
        }
    }

    fn close_failed_document(&mut self, path: &Path, synchronization: TextSynchronization) {
        let Ok(uri) = file_uri(path) else {
            return;
        };
        if self.documents.contains_key(&uri) {
            self.remove(&uri);
            if synchronization == TextSynchronization::OpenClose {
                self.pending_events
                    .push(SynchronizationEvent::DidClose { uri });
            }
        }
    }

    fn allocate_version(&mut self, uri: &str) -> i64 {
        let version = self.next_versions.entry(uri.to_owned()).or_insert(0);
        *version = version.saturating_add(1);
        *version
    }

    fn evict_to_limits(
        &mut self,
        active_uri: &str,
        synchronization: TextSynchronization,
    ) -> Result<(Vec<SynchronizationEvent>, Vec<String>), ContractFailure> {
        let mut events = Vec::new();
        let mut evicted = Vec::new();
        while self.documents.len() > self.max_open_documents
            || self.total_text_bytes > self.max_total_text_bytes
        {
            let candidate = self
                .documents
                .iter()
                .filter(|(uri, _)| uri.as_str() != active_uri)
                .min_by_key(|(_, open)| open.last_used)
                .map(|(uri, _)| uri.clone());
            let Some(uri) = candidate else {
                let active = self.documents.get(active_uri).unwrap();
                let failure = document_too_large(
                    &active.snapshot.path,
                    &active.snapshot.uri,
                    active.snapshot.text.len() as u64,
                    self.max_total_text_bytes,
                );
                self.remove(active_uri);
                if synchronization == TextSynchronization::OpenClose {
                    self.pending_events.push(SynchronizationEvent::DidClose {
                        uri: active_uri.to_owned(),
                    });
                }
                return Err(failure);
            };
            self.remove(&uri);
            if synchronization == TextSynchronization::OpenClose {
                events.push(SynchronizationEvent::DidClose { uri: uri.clone() });
            }
            evicted.push(uri);
        }
        Ok((events, evicted))
    }
}

/// Selects the one canonical Workspace without inferring a Git root.
pub(crate) fn select_workspace(
    explicit: Option<&Path>,
    target_files: &[PathBuf],
    current_directory: &Path,
) -> Result<Workspace, ContractFailure> {
    if let Some(explicit) = explicit {
        return workspace_from_path(explicit, true);
    }
    let current_directory = dunce::canonicalize(current_directory)
        .map_err(|_| workspace_failure("The current directory cannot be resolved.", &[]))?;
    let starts = if target_files.is_empty() {
        vec![current_directory.clone()]
    } else {
        target_files
            .iter()
            .map(|path| canonical_search_start(path, &current_directory))
            .collect::<Result<Vec<_>, _>>()?
    };
    let candidates = starts
        .iter()
        .map(|start| nearest_project_root(start).unwrap_or_else(|| current_directory.clone()))
        .collect::<BTreeSet<_>>();
    if candidates.len() > 1 {
        return Err(workspace_failure(
            "Inputs resolve to different project configuration roots.",
            &candidates.into_iter().collect::<Vec<_>>(),
        ));
    }
    let root = candidates.into_iter().next().unwrap_or(current_directory);
    workspace_from_path(&root, false)
}

pub(crate) fn workspace_from_path(
    path: &Path,
    explicitly_selected: bool,
) -> Result<Workspace, ContractFailure> {
    let root = dunce::canonicalize(path)
        .map_err(|_| workspace_failure("The Workspace path cannot be resolved.", &[path.into()]))?;
    if !root.is_dir() {
        return Err(workspace_failure(
            "The Workspace path is not a directory.",
            &[root],
        ));
    }
    let uri = directory_uri(&root)?;
    Ok(Workspace {
        root,
        uri,
        explicitly_selected,
    })
}

pub(crate) fn file_uri(path: &Path) -> Result<String, ContractFailure> {
    Url::from_file_path(path)
        .map(|uri| uri.to_string())
        .map_err(|()| {
            workspace_failure(
                "A file path cannot be represented as a URI.",
                &[path.into()],
            )
        })
}

pub(crate) fn directory_uri(path: &Path) -> Result<String, ContractFailure> {
    Url::from_directory_path(path)
        .map(|uri| uri.to_string())
        .map_err(|()| {
            workspace_failure(
                "A Workspace path cannot be represented as a URI.",
                &[path.into()],
            )
        })
}

pub(crate) fn validate_document_scope(
    workspace: &Workspace,
    path: &Path,
    server_explicitly_selected: bool,
    mutation: bool,
) -> Result<PathBuf, ContractFailure> {
    let canonical = dunce::canonicalize(path)
        .map_err(|_| workspace_failure("A target path cannot be resolved.", &[path.into()]))?;
    if canonical.starts_with(&workspace.root)
        || (!mutation && workspace.explicitly_selected && server_explicitly_selected)
    {
        Ok(canonical)
    } else {
        Err(workspace_failure(
            if mutation {
                "A Mutation target is outside the Workspace."
            } else {
                "An outside-Workspace Document requires explicit Workspace and server selection."
            },
            &[canonical],
        ))
    }
}

pub(crate) fn resolve_source_position(
    file: &Path,
    text: &str,
    line: u32,
    scalar_column: u32,
    encoding: PositionEncoding,
) -> Result<ResolvedPosition, ContractFailure> {
    let (line_start, line_text) = line_slice(text, line).ok_or_else(|| {
        invalid_position_failure(
            file,
            line,
            Some(scalar_column),
            "line is outside the Document",
        )
    })?;
    let byte_in_line = byte_offset_for_scalar(line_text, scalar_column).ok_or_else(|| {
        invalid_position_failure(
            file,
            line,
            Some(scalar_column),
            "column is outside the Document line",
        )
    })?;
    let character = match encoding {
        PositionEncoding::Utf8 => byte_in_line,
        PositionEncoding::Utf16 => line_text[..byte_in_line].encode_utf16().count(),
    };
    Ok(ResolvedPosition {
        byte_offset: line_start + byte_in_line,
        protocol: ProtocolPosition {
            line,
            character: u32::try_from(character).map_err(|_| {
                invalid_position_failure(file, line, Some(scalar_column), "position is too large")
            })?,
        },
    })
}

pub(crate) fn protocol_position_to_offset(
    file: &Path,
    text: &str,
    position: ProtocolPosition,
    encoding: PositionEncoding,
) -> Result<usize, ContractFailure> {
    let (line_start, line_text) = line_slice(text, position.line).ok_or_else(|| {
        invalid_position_failure(file, position.line, None, "line is outside the Document")
    })?;
    let wanted = usize::try_from(position.character).unwrap_or(usize::MAX);
    let byte_in_line = match encoding {
        PositionEncoding::Utf8 => line_text
            .is_char_boundary(wanted)
            .then_some(wanted)
            .filter(|offset| *offset <= line_text.len()),
        PositionEncoding::Utf16 => {
            let mut units = 0;
            let mut found = None;
            for (offset, character) in line_text.char_indices() {
                if units == wanted {
                    found = Some(offset);
                    break;
                }
                units += character.len_utf16();
                if units > wanted {
                    return Err(invalid_position_failure(
                        file,
                        position.line,
                        None,
                        "UTF-16 position splits a surrogate pair",
                    ));
                }
            }
            if found.is_none() && units == wanted {
                found = Some(line_text.len());
            }
            found
        }
    }
    .ok_or_else(|| {
        invalid_position_failure(
            file,
            position.line,
            None,
            "protocol position is outside the Document line",
        )
    })?;
    Ok(line_start + byte_in_line)
}

pub(crate) fn validate_snapshot_after_query(
    snapshot: &DocumentSnapshot,
    max_document_bytes: u64,
    server_result: &Value,
) -> Result<(), ContractFailure> {
    let (_, _, _, after_digest, _) = read_document(&snapshot.path, max_document_bytes)?;
    if after_digest == snapshot.digest {
        return Ok(());
    }
    Err(ContractFailure {
        exit_code: 5,
        category: "query",
        code: "document_changed_during_query",
        message: "A synchronized Document changed while the server request was running.".to_owned(),
        stage: "validate_result",
        delivery: "sent",
        retry: "after_change",
        data: json!({
            "uri": snapshot.uri,
            "beforeDigest": snapshot.digest,
            "afterDigest": after_digest,
            "serverResult": server_result
        }),
    })
}

fn read_document(
    path: &Path,
    max_document_bytes: u64,
) -> Result<(PathBuf, String, String, String, FileIdentity), ContractFailure> {
    let canonical = dunce::canonicalize(path).map_err(|error| {
        document_failure(
            "document_read_failed",
            path,
            5,
            json!({"reason": error.to_string(), "osCode": error.raw_os_error()}),
        )
    })?;
    let uri = file_uri(&canonical)?;
    let mut observations = Vec::new();
    for _ in 0..2 {
        match read_document_once(&canonical, max_document_bytes, &uri) {
            Ok(value) => return Ok((canonical, uri, value.0, value.1, value.2)),
            Err(ReadAttemptError::Changed(before, after)) => {
                observations.push((before, after));
            }
            Err(ReadAttemptError::Failure(failure)) => return Err(failure),
        }
    }
    let (before, after) = observations.pop().unwrap();
    Err(ContractFailure {
        exit_code: 5,
        category: "query",
        code: "document_changed_while_reading",
        message: "The Document changed repeatedly while it was being read.".to_owned(),
        stage: "synchronize",
        delivery: "not_sent",
        retry: "safe",
        data: json!({"uri": uri, "before": before, "after": after}),
    })
}

enum ReadAttemptError {
    Changed(Value, Value),
    Failure(ContractFailure),
}

fn read_document_once(
    path: &Path,
    max_document_bytes: u64,
    uri: &str,
) -> Result<(String, String, FileIdentity), ReadAttemptError> {
    let mut file = File::open(path).map_err(|error| {
        ReadAttemptError::Failure(document_failure(
            "document_read_failed",
            path,
            5,
            json!({"uri": uri, "reason": error.to_string(), "osCode": error.raw_os_error()}),
        ))
    })?;
    let before_metadata = file.metadata().map_err(|error| {
        ReadAttemptError::Failure(document_failure(
            "document_read_failed",
            path,
            5,
            json!({"uri": uri, "reason": error.to_string(), "osCode": error.raw_os_error()}),
        ))
    })?;
    if !before_metadata.is_file() {
        return Err(ReadAttemptError::Failure(document_failure(
            "document_read_failed",
            path,
            5,
            json!({"uri": uri, "reason": "the path is not a regular file"}),
        )));
    }
    if before_metadata.len() > max_document_bytes {
        return Err(ReadAttemptError::Failure(document_too_large(
            path,
            uri,
            before_metadata.len(),
            max_document_bytes,
        )));
    }
    let before = file_identity(&file, &before_metadata).map_err(|error| {
        ReadAttemptError::Failure(document_failure(
            "document_read_failed",
            path,
            5,
            json!({"uri": uri, "reason": error.to_string(), "osCode": error.raw_os_error()}),
        ))
    })?;
    let capacity = usize::try_from(before_metadata.len()).unwrap_or(usize::MAX);
    let mut bytes = Vec::with_capacity(capacity.min(1024 * 1024));
    (&mut file)
        .take(max_document_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ReadAttemptError::Failure(document_failure(
                "document_read_failed",
                path,
                5,
                json!({"uri": uri, "reason": error.to_string(), "osCode": error.raw_os_error()}),
            ))
        })?;
    if bytes.len() as u64 > max_document_bytes {
        return Err(ReadAttemptError::Failure(document_too_large(
            path,
            uri,
            bytes.len() as u64,
            max_document_bytes,
        )));
    }
    let after_handle = file
        .metadata()
        .and_then(|metadata| file_identity(&file, &metadata))
        .map_err(|error| {
            ReadAttemptError::Failure(document_failure(
                "document_read_failed",
                path,
                5,
                json!({"uri": uri, "reason": error.to_string(), "osCode": error.raw_os_error()}),
            ))
        })?;
    let after_path_file = File::open(path).map_err(|error| {
        ReadAttemptError::Failure(document_failure(
            "document_read_failed",
            path,
            5,
            json!({"uri": uri, "reason": error.to_string(), "osCode": error.raw_os_error()}),
        ))
    })?;
    let after_path = after_path_file
        .metadata()
        .and_then(|metadata| file_identity(&after_path_file, &metadata))
        .map_err(|error| {
            ReadAttemptError::Failure(document_failure(
                "document_read_failed",
                path,
                5,
                json!({"uri": uri, "reason": error.to_string(), "osCode": error.raw_os_error()}),
            ))
        })?;
    if before != after_handle || before != after_path || before.length != bytes.len() as u64 {
        return Err(ReadAttemptError::Changed(
            identity_json(&before),
            identity_json(&after_path),
        ));
    }
    let digest = digest_raw_bytes(&bytes);
    let text = String::from_utf8(bytes).map_err(|_| {
        ReadAttemptError::Failure(document_failure(
            "document_non_utf8",
            path,
            5,
            json!({"uri": uri}),
        ))
    })?;
    Ok((text, digest, before))
}

fn modified_nanos(metadata: &Metadata) -> Option<u128> {
    metadata
        .modified()
        .ok()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos())
}

#[cfg(unix)]
fn file_identity(_file: &File, metadata: &Metadata) -> io::Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt;

    Ok(FileIdentity {
        length: metadata.len(),
        modified_nanos: modified_nanos(metadata),
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn file_identity(file: &File, metadata: &Metadata) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(file.as_raw_handle().cast(), &mut information) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let file_index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);

    Ok(FileIdentity {
        length: metadata.len(),
        modified_nanos: modified_nanos(metadata),
        volume_serial: Some(information.dwVolumeSerialNumber),
        file_index: Some(file_index),
    })
}

#[cfg(not(any(unix, windows)))]
fn file_identity(_file: &File, metadata: &Metadata) -> io::Result<FileIdentity> {
    Ok(FileIdentity {
        length: metadata.len(),
        modified_nanos: modified_nanos(metadata),
    })
}

fn identity_json(identity: &FileIdentity) -> Value {
    let mut value = json!({
        "length": identity.length,
        "modifiedNanos": identity.modified_nanos.map(|value| value.to_string())
    });
    #[cfg(unix)]
    {
        value["device"] = json!(identity.device);
        value["inode"] = json!(identity.inode);
    }
    #[cfg(windows)]
    {
        value["volumeSerial"] = json!(identity.volume_serial);
        value["fileIndex"] = json!(identity.file_index);
    }
    value
}

fn canonical_search_start(
    path: &Path,
    current_directory: &Path,
) -> Result<PathBuf, ContractFailure> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        current_directory.join(path)
    };
    let path = dunce::canonicalize(&path)
        .map_err(|_| workspace_failure("A target path cannot be resolved.", &[path]))?;
    Ok(if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(&path).to_path_buf()
    })
}

fn nearest_project_root(start: &Path) -> Option<PathBuf> {
    start
        .ancestors()
        .find(|ancestor| ancestor.join(".lspc.toml").is_file())
        .map(Path::to_path_buf)
}

fn line_slice(text: &str, wanted_line: u32) -> Option<(usize, &str)> {
    let mut line = 0_u32;
    let mut start = 0;
    for (offset, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            if line == wanted_line {
                let end = if offset > start && text.as_bytes()[offset - 1] == b'\r' {
                    offset - 1
                } else {
                    offset
                };
                return Some((start, &text[start..end]));
            }
            line = line.saturating_add(1);
            start = offset + 1;
        }
    }
    (line == wanted_line).then_some((start, &text[start..]))
}

fn byte_offset_for_scalar(line: &str, scalar_column: u32) -> Option<usize> {
    let wanted = usize::try_from(scalar_column).ok()?;
    if wanted == 0 {
        return Some(0);
    }
    line.char_indices()
        .nth(wanted)
        .map(|(offset, _)| offset)
        .or_else(|| (line.chars().count() == wanted).then_some(line.len()))
}

fn invalid_position_failure(
    file: &Path,
    line: u32,
    column: Option<u32>,
    reason: &str,
) -> ContractFailure {
    let mut data = json!({"file": file, "line": line, "reason": reason});
    if let Some(column) = column {
        data["column"] = json!(column);
    }
    ContractFailure {
        exit_code: 2,
        category: "input",
        code: "invalid_source_position",
        message: "A source position is invalid.".to_owned(),
        stage: "synchronize",
        delivery: "not_sent",
        retry: "after_change",
        data,
    }
}

fn workspace_failure(reason: &str, paths: &[PathBuf]) -> ContractFailure {
    let mut data = json!({"reason": reason});
    if !paths.is_empty() {
        data["paths"] = json!(paths);
    }
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "workspace_selection_failed",
        message: "A single Workspace could not be selected.".to_owned(),
        stage: "resolve_workspace",
        delivery: "not_sent",
        retry: "after_change",
        data,
    }
}

fn document_failure(
    code: &'static str,
    path: &Path,
    exit_code: u8,
    mut extra: Value,
) -> ContractFailure {
    let uri = file_uri(path).unwrap_or_default();
    extra["path"] = json!(path);
    if extra.get("uri").is_none() {
        extra["uri"] = json!(uri);
    }
    ContractFailure {
        exit_code,
        category: "query",
        code,
        message: "A Document could not be synchronized.".to_owned(),
        stage: "synchronize",
        delivery: "not_sent",
        retry: "after_change",
        data: extra,
    }
}

fn document_too_large(path: &Path, uri: &str, size: u64, limit: u64) -> ContractFailure {
    document_failure(
        "document_too_large",
        path,
        5,
        json!({"uri": uri, "size": size, "limit": limit}),
    )
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use tempfile::TempDir;

    use super::*;

    #[test]
    fn workspace_selection_uses_nearest_configuration_without_git() {
        let root = TempDir::new().unwrap();
        fs::write(root.path().join(".lspc.toml"), "version = 1").unwrap();
        let nested = root.path().join("src/nested");
        fs::create_dir_all(&nested).unwrap();
        let file = nested.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();

        let workspace = select_workspace(None, &[file], &nested).unwrap();
        assert_eq!(workspace.root, dunce::canonicalize(root.path()).unwrap());
        assert!(!workspace.explicitly_selected);
        assert!(workspace.uri.starts_with("file:"));
    }

    #[test]
    fn workspace_selection_rejects_different_implied_roots() {
        let current = TempDir::new().unwrap();
        let project = TempDir::new().unwrap();
        fs::write(project.path().join(".lspc.toml"), "version = 1").unwrap();
        let project_file = project.path().join("main.rs");
        let unrelated_file = current.path().join("other.rs");
        fs::write(&project_file, "").unwrap();
        fs::write(&unrelated_file, "").unwrap();

        let failure =
            select_workspace(None, &[project_file, unrelated_file], current.path()).unwrap_err();
        assert_eq!(failure.code, "workspace_selection_failed");
    }

    #[test]
    fn unicode_scalar_positions_convert_to_utf8_and_utf16() {
        let text = "zero\r\na😀b\n";
        let file = Path::new("unicode.rs");
        let utf8 = resolve_source_position(file, text, 1, 2, PositionEncoding::Utf8).unwrap();
        let utf16 = resolve_source_position(file, text, 1, 2, PositionEncoding::Utf16).unwrap();
        assert_eq!(utf8.protocol.character, 5);
        assert_eq!(utf16.protocol.character, 3);
        assert_eq!(utf8.byte_offset, utf16.byte_offset);
        assert_eq!(
            protocol_position_to_offset(file, text, utf16.protocol, PositionEncoding::Utf16)
                .unwrap(),
            utf16.byte_offset
        );
        assert!(
            protocol_position_to_offset(
                file,
                text,
                ProtocolPosition {
                    line: 1,
                    character: 2
                },
                PositionEncoding::Utf16
            )
            .is_err()
        );
    }

    #[test]
    fn refresh_closes_and_reopens_changed_documents_and_evicts_lru() {
        let root = TempDir::new().unwrap();
        let first = root.path().join("first.rs");
        let second = root.path().join("second.rs");
        fs::write(&first, "one").unwrap();
        fs::write(&second, "two").unwrap();
        let mut store = DocumentStore::new(1, 1024, 1024);

        let opened = store
            .refresh(&first, "rust", TextSynchronization::OpenClose)
            .unwrap();
        assert_eq!(opened.events.len(), 1);
        fs::File::create(&first)
            .unwrap()
            .write_all(b"changed")
            .unwrap();
        let changed = store
            .refresh(&first, "rust", TextSynchronization::OpenClose)
            .unwrap();
        assert_eq!(changed.events.len(), 2);
        assert_eq!(changed.snapshot.version, 2);

        let second = store
            .refresh(&second, "rust", TextSynchronization::OpenClose)
            .unwrap();
        assert_eq!(second.evicted_uris.len(), 1);
        assert_eq!(store.snapshots().len(), 1);
    }

    #[test]
    fn oversized_and_non_utf8_documents_fail_before_open() {
        let root = TempDir::new().unwrap();
        let large = root.path().join("large");
        fs::write(&large, b"1234").unwrap();
        let mut store = DocumentStore::new(1, 3, 3);
        assert_eq!(
            store
                .refresh(&large, "text", TextSynchronization::None)
                .unwrap_err()
                .code,
            "document_too_large"
        );

        let binary = root.path().join("binary");
        fs::write(&binary, [0xff]).unwrap();
        assert_eq!(
            store
                .refresh(&binary, "text", TextSynchronization::None)
                .unwrap_err()
                .code,
            "document_non_utf8"
        );
    }

    #[test]
    fn unreadable_open_document_is_closed_and_removed() {
        let root = TempDir::new().unwrap();
        let file = root.path().join("removed.rs");
        fs::write(&file, "one").unwrap();
        let mut store = DocumentStore::new(2, 1024, 2048);
        let opened = store
            .refresh(&file, "rust", TextSynchronization::OpenClose)
            .unwrap();
        fs::remove_file(&file).unwrap();

        assert!(
            store
                .refresh(
                    &opened.snapshot.path,
                    "rust",
                    TextSynchronization::OpenClose
                )
                .is_err()
        );
        assert!(store.snapshots().is_empty());
        assert!(matches!(
            store.drain_pending_events().as_slice(),
            [SynchronizationEvent::DidClose { .. }]
        ));
    }
}
