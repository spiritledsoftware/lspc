#![allow(clippy::result_large_err)]

use std::{
    collections::BTreeSet,
    fs::{self, File},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions as CapabilityOpenOptions};
use serde_json::{Value, json};

use crate::{
    canonical_value::digest_raw_bytes,
    configuration::{MutationSettings, PreviewSettings, ReceiptSettings},
    contract::ContractFailure,
    state_permissions,
    workspace::PositionEncoding,
};

use super::{
    planner::{
        CanonicalOperation, ManifestEntry, ResourceKind, WorkspaceEditPlanner, WorkspaceEditProblem,
    },
    state::{
        BackupEntry, MUTATION_STATE_VERSION, MutationStateStore, ReceiptRecord, StoredPreview,
        TransactionRecord, TransactionState, manifest_digest, now_rfc3339,
    },
};

type ReauthorizePreview<'a> = dyn Fn(&StoredPreview) -> Result<Vec<Value>, ContractFailure> + 'a;

pub(crate) struct ApplicationContext<'a> {
    pub(crate) store: &'a MutationStateStore,
    pub(crate) preview_limits: &'a PreviewSettings,
    pub(crate) receipt_limits: &'a ReceiptSettings,
    pub(crate) mutation_limits: &'a MutationSettings,
    pub(crate) reauthorize: Option<&'a ReauthorizePreview<'a>>,
}

/// Applies one exact Preview under the Workspace lock and records at-most-once completion.
pub(crate) fn apply_preview(
    context: &ApplicationContext<'_>,
    preview_id: &str,
) -> Result<Value, ContractFailure> {
    if let Some(receipt) = context.store.already_applied(preview_id)?
        && receipt.receipt.outcome == "applied"
    {
        return Ok(application_success(&receipt.receipt, "already_applied"));
    };
    let mut stored = context.store.reserve_preview(preview_id)?;
    let lock_file = context
        .store
        .open_application_lock(&stored.preview.workspace_uri)?;
    if let Err(failure) = lock_workspace(
        &lock_file,
        &stored.preview.workspace_uri,
        &context.mutation_limits.application_lock_timeout,
    ) {
        let _ = context.store.release_preview(&mut stored);
        return Err(failure);
    }
    if let Some(reauthorize) = context.reauthorize {
        let stale_reasons = match reauthorize(&stored) {
            Ok(reasons) => reasons,
            Err(failure) => {
                let _ = context.store.release_preview(&mut stored);
                return Err(failure);
            }
        };
        if !stale_reasons.is_empty() {
            let _ = context.store.release_preview(&mut stored);
            return Err(preview_stale_failure(&stored, stale_reasons));
        }
    }
    for transaction in context.store.list_transactions()? {
        match transaction {
            Ok(transaction) if transaction.workspace_uri == stored.preview.workspace_uri => {
                let _ = context.store.release_preview(&mut stored);
                return Err(recovery_required_failure(&transaction));
            }
            Ok(_) => {}
            Err(evidence) => {
                let _ = context.store.release_preview(&mut stored);
                return Err(ContractFailure {
                    exit_code: 7,
                    category: "recovery",
                    code: "recovery_evidence_invalid",
                    message: "Corrupt Recovery evidence blocks Workspace Application.".to_owned(),
                    stage: "recover",
                    delivery: "not_applicable",
                    retry: "never",
                    data: json!({
                        "transactionId": transaction_id_from_evidence(&evidence),
                        "problems": evidence["problems"]
                    }),
                });
            }
        }
    }
    context
        .store
        .ensure_receipt_capacity(context.receipt_limits)?;
    let planner = WorkspaceEditPlanner::open(
        &stored.workspace_path,
        &stored.preview.workspace_uri,
        parse_position_encoding(&stored.preview.position_encoding),
        context.preview_limits,
        context.mutation_limits,
    )
    .map_err(|problem| unsupported_filesystem_failure(&stored.preview.workspace_uri, &[problem]))?;
    let current = planner
        .inspect_manifest_paths(
            &stored
                .preview
                .plan
                .before_manifest
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
        )
        .map_err(|problems| {
            unsupported_filesystem_failure(&stored.preview.workspace_uri, &problems)
        })?;
    let stale_reasons = manifest_mismatches(&stored.preview.plan.before_manifest, &current);
    if !stale_reasons.is_empty() {
        let _ = context.store.release_preview(&mut stored);
        return Err(ContractFailure {
            exit_code: 6,
            category: "mutation",
            code: "preview_stale",
            message: "The Preview no longer matches the Workspace filesystem.".to_owned(),
            stage: "reserve",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "previewId": preview_id,
                "reasons": stale_reasons,
                "preconditions": stored.preview.preconditions
            }),
        });
    }

    let transaction_id = context.store.new_transaction_id()?;
    let artifact_directory = stored
        .workspace_path
        .join(format!(".lspc-{transaction_id}"));
    let mut transaction = TransactionRecord {
        format_version: MUTATION_STATE_VERSION,
        transaction_id: transaction_id.clone(),
        preview_id: preview_id.to_owned(),
        receipt_id: preview_id.to_owned(),
        workspace_path: stored.workspace_path.clone(),
        workspace_uri: stored.preview.workspace_uri.clone(),
        state: TransactionState::Staged,
        started_at: now_rfc3339(),
        artifact_directory: artifact_directory.clone(),
        backups: planned_backups(&stored.preview.plan.before_manifest, &artifact_directory),
        before_manifest: stored.preview.plan.before_manifest.clone(),
        intended_manifest: stored.preview.plan.intended_manifest.clone(),
        observed_manifest: current,
        manifest_digest: manifest_digest(&stored.preview.plan.before_manifest),
        cleanup_pending: false,
    };
    context.store.write_transaction(&transaction)?;
    if let Err(failure) = stage_transaction_backups(&transaction, context.mutation_limits) {
        let _ = cleanup_transaction_artifacts(&transaction);
        let _ = context.store.remove_transaction(&transaction_id);
        let _ = context.store.release_preview(&mut stored);
        return Err(failure);
    }
    transaction.state = TransactionState::Committing;
    context.store.write_transaction(&transaction)?;
    let started_at = transaction.started_at.clone();
    let commit_result = commit_operations(&planner, &stored.preview.plan.operations);
    let intended_paths = stored
        .preview
        .plan
        .intended_manifest
        .iter()
        .map(|entry| entry.path.clone())
        .collect::<Vec<_>>();
    let observed = planner
        .inspect_manifest_paths(&intended_paths)
        .unwrap_or_default();
    let manifest_ok =
        manifest_mismatches(&stored.preview.plan.intended_manifest, &observed).is_empty();
    if commit_result.is_ok() && manifest_ok {
        transaction.observed_manifest.clone_from(&observed);
        let cleanup_pending = cleanup_transaction_artifacts(&transaction).is_err();
        transaction.cleanup_pending = cleanup_pending;
        let receipt = ReceiptRecord {
            receipt_id: preview_id.to_owned(),
            kind: "receipt".to_owned(),
            transaction_id: transaction_id.clone(),
            workspace_uri: stored.preview.workspace_uri.clone(),
            server: stored.preview.server.clone(),
            session_identity: Some(stored.preview.session_identity.clone()),
            preview_id: Some(preview_id.to_owned()),
            linked_receipt_id: None,
            preauthorized: false,
            started_at,
            completed_at: now_rfc3339(),
            outcome: "applied".to_owned(),
            filesystem_state: "changed".to_owned(),
            summary: stored.preview.summary.clone(),
            before_manifest: stored.preview.plan.before_manifest.clone(),
            intended_manifest: stored.preview.plan.intended_manifest.clone(),
            observed_manifest: observed.clone(),
            session_synchronized: false,
            cleanup_pending,
            durability: durability_value(),
            manifest_digest: manifest_digest(&observed),
            failure_stage: None,
            failed_change: None,
        };
        context.store.convert_preview_to_receipt(
            preview_id,
            receipt.clone(),
            context.receipt_limits,
        )?;
        if !cleanup_pending {
            context.store.remove_transaction(&transaction_id)?;
        } else {
            context.store.write_transaction(&transaction)?;
        }
        return Ok(application_success(&receipt, "applied"));
    }

    transaction.observed_manifest = observed;
    match rollback_transaction(&transaction, &planner) {
        Ok(restored) => {
            let cleanup_pending = cleanup_transaction_artifacts(&transaction).is_err();
            let receipt = ReceiptRecord {
                receipt_id: preview_id.to_owned(),
                kind: "receipt".to_owned(),
                transaction_id: transaction_id.clone(),
                workspace_uri: stored.preview.workspace_uri.clone(),
                server: stored.preview.server.clone(),
                session_identity: Some(stored.preview.session_identity.clone()),
                preview_id: Some(preview_id.to_owned()),
                linked_receipt_id: None,
                preauthorized: false,
                started_at,
                completed_at: now_rfc3339(),
                outcome: "rolled_back".to_owned(),
                filesystem_state: "unchanged".to_owned(),
                summary: stored.preview.summary.clone(),
                before_manifest: stored.preview.plan.before_manifest.clone(),
                intended_manifest: stored.preview.plan.intended_manifest.clone(),
                observed_manifest: restored.clone(),
                session_synchronized: false,
                cleanup_pending,
                durability: durability_value(),
                manifest_digest: manifest_digest(&restored),
                failure_stage: Some("commit".to_owned()),
                failed_change: commit_result.err().map(|failure| failure.operation_index),
            };
            context.store.convert_preview_to_receipt(
                preview_id,
                receipt.clone(),
                context.receipt_limits,
            )?;
            if !cleanup_pending {
                context.store.remove_transaction(&transaction_id)?;
            }
            Err(ContractFailure {
                exit_code: 6,
                category: "mutation",
                code: "rolled_back",
                message:
                    "The Application failed and the pre-Application filesystem state was restored."
                        .to_owned(),
                stage: "rollback",
                delivery: "not_applicable",
                retry: "after_change",
                data: json!({
                    "previewId": preview_id,
                    "receiptId": preview_id,
                    "transactionId": transaction_id,
                    "failureStage": "commit",
                    "manifest": restored
                }),
            })
        }
        Err(_) => {
            transaction.state = TransactionState::RecoveryRequired;
            transaction.observed_manifest = planner
                .inspect_manifest_paths(&transaction_paths(&transaction))
                .unwrap_or_else(|_| inspect_paths_best_effort(&transaction.before_manifest));
            transaction.manifest_digest = manifest_digest(&transaction.observed_manifest);
            context.store.write_transaction(&transaction)?;
            let receipt = ReceiptRecord {
                receipt_id: preview_id.to_owned(),
                kind: "receipt".to_owned(),
                transaction_id: transaction_id.clone(),
                workspace_uri: stored.preview.workspace_uri.clone(),
                server: stored.preview.server.clone(),
                session_identity: Some(stored.preview.session_identity.clone()),
                preview_id: Some(preview_id.to_owned()),
                linked_receipt_id: None,
                preauthorized: false,
                started_at,
                completed_at: now_rfc3339(),
                outcome: "recovery_required".to_owned(),
                filesystem_state: "partial".to_owned(),
                summary: stored.preview.summary.clone(),
                before_manifest: stored.preview.plan.before_manifest.clone(),
                intended_manifest: stored.preview.plan.intended_manifest.clone(),
                observed_manifest: transaction.observed_manifest.clone(),
                session_synchronized: false,
                cleanup_pending: true,
                durability: durability_value(),
                manifest_digest: transaction.manifest_digest.clone(),
                failure_stage: Some("rollback".to_owned()),
                failed_change: commit_result.err().map(|failure| failure.operation_index),
            };
            context.store.convert_preview_to_receipt(
                preview_id,
                receipt,
                context.receipt_limits,
            )?;
            Err(recovery_required_failure(&transaction))
        }
    }
}

/// Rolls a recovery transaction back only while its observed manifest still matches.
pub(crate) fn recover_rollback(
    store: &MutationStateStore,
    transaction_id: &str,
    supplied_manifest_digest: &str,
    preview_limits: &PreviewSettings,
    receipt_limits: &ReceiptSettings,
    mutation_limits: &MutationSettings,
) -> Result<Value, ContractFailure> {
    recover_transaction(
        store,
        transaction_id,
        supplied_manifest_digest,
        false,
        preview_limits,
        receipt_limits,
        mutation_limits,
    )
}

/// Accepts the exact current recovery manifest without replaying filesystem writes.
pub(crate) fn recover_accept_current(
    store: &MutationStateStore,
    transaction_id: &str,
    supplied_manifest_digest: &str,
    preview_limits: &PreviewSettings,
    receipt_limits: &ReceiptSettings,
    mutation_limits: &MutationSettings,
) -> Result<Value, ContractFailure> {
    recover_transaction(
        store,
        transaction_id,
        supplied_manifest_digest,
        true,
        preview_limits,
        receipt_limits,
        mutation_limits,
    )
}

fn recover_transaction(
    store: &MutationStateStore,
    transaction_id: &str,
    supplied_manifest_digest: &str,
    accept_current: bool,
    preview_limits: &PreviewSettings,
    receipt_limits: &ReceiptSettings,
    mutation_limits: &MutationSettings,
) -> Result<Value, ContractFailure> {
    let mut transaction = store.read_transaction(transaction_id)?;
    if supplied_manifest_digest != transaction.manifest_digest {
        return Err(ContractFailure {
            exit_code: 7,
            category: "recovery",
            code: "recovery_manifest_mismatch",
            message: "The supplied Recovery manifest digest does not match the journal.".to_owned(),
            stage: "recover",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "transactionId": transaction_id,
                "expectedDigest": transaction.manifest_digest,
                "actualDigest": supplied_manifest_digest
            }),
        });
    }
    store.ensure_receipt_capacity(receipt_limits)?;
    let lock = store.open_application_lock(&transaction.workspace_uri)?;
    lock_workspace(
        &lock,
        &transaction.workspace_uri,
        &mutation_limits.application_lock_timeout,
    )?;
    let planner = WorkspaceEditPlanner::open(
        &transaction.workspace_path,
        &transaction.workspace_uri,
        PositionEncoding::Utf8,
        preview_limits,
        mutation_limits,
    )
    .map_err(|problem| unsupported_filesystem_failure(&transaction.workspace_uri, &[problem]))?;
    let current = planner
        .inspect_manifest_paths(&transaction_paths(&transaction))
        .map_err(|problems| {
            unsupported_filesystem_failure(&transaction.workspace_uri, &problems)
        })?;
    let current_digest = manifest_digest(&current);
    if current_digest != transaction.manifest_digest {
        return Err(ContractFailure {
            exit_code: 7,
            category: "recovery",
            code: "recovery_conflict",
            message: "The filesystem changed after Recovery was recorded.".to_owned(),
            stage: "recover",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "transactionId": transaction_id,
                "paths": current.iter().map(|entry| &entry.path).collect::<Vec<_>>(),
                "intended": transaction.observed_manifest,
                "observed": current
            }),
        });
    }
    let outcome = if accept_current {
        "accepted_current"
    } else if transaction.state == TransactionState::Staged
        && rollback_manifest_mismatches(&transaction.before_manifest, &current).is_empty()
    {
        "restored"
    } else {
        rollback_transaction(&transaction, &planner).map_err(|failure| ContractFailure {
            exit_code: 7,
            category: "recovery",
            code: "recovery_failed",
            message: "Recovery rollback could not restore the recorded filesystem state."
                .to_owned(),
            stage: "recover",
            delivery: "not_applicable",
            retry: "after_change",
            data: json!({
                "transactionId": transaction_id,
                "failureStage": failure,
                "intended": transaction.before_manifest,
                "observed": inspect_paths_best_effort(&transaction.before_manifest)
            }),
        })?;
        "restored"
    };
    let final_manifest = if accept_current {
        current
    } else {
        planner
            .inspect_manifest_paths(&transaction_paths(&transaction))
            .unwrap_or_else(|_| inspect_paths_best_effort(&transaction.before_manifest))
    };
    let recovery_receipt_id = store.new_receipt_id()?;
    let original = store.read_receipt(&transaction.receipt_id).ok();
    let pending_preview = store.read_preview(&transaction.preview_id).ok();
    let receipt = ReceiptRecord {
        receipt_id: recovery_receipt_id.clone(),
        kind: "recovery_receipt".to_owned(),
        transaction_id: transaction_id.to_owned(),
        workspace_uri: transaction.workspace_uri.clone(),
        server: original
            .as_ref()
            .and_then(|record| record.receipt.server.clone())
            .or_else(|| pending_preview.as_ref()?.preview.server.clone()),
        session_identity: original
            .as_ref()
            .and_then(|record| record.receipt.session_identity.clone())
            .or_else(|| Some(pending_preview.as_ref()?.preview.session_identity.clone())),
        preview_id: original
            .as_ref()
            .and_then(|record| record.receipt.preview_id.clone())
            .or_else(|| Some(transaction.preview_id.clone())),
        linked_receipt_id: original
            .as_ref()
            .map(|record| record.receipt.receipt_id.clone()),
        preauthorized: false,
        started_at: now_rfc3339(),
        completed_at: now_rfc3339(),
        outcome: outcome.to_owned(),
        filesystem_state: if accept_current {
            "changed"
        } else {
            "unchanged"
        }
        .to_owned(),
        summary: original
            .as_ref()
            .map(|record| record.receipt.summary.clone())
            .or_else(|| {
                pending_preview
                    .as_ref()
                    .map(|record| record.preview.summary.clone())
            })
            .unwrap_or_default(),
        before_manifest: transaction.observed_manifest.clone(),
        intended_manifest: final_manifest.clone(),
        observed_manifest: final_manifest.clone(),
        session_synchronized: false,
        cleanup_pending: false,
        durability: durability_value(),
        manifest_digest: manifest_digest(&final_manifest),
        failure_stage: None,
        failed_change: None,
    };
    store.write_receipt(receipt, receipt_limits)?;
    store.retire_preview_after_recovery(&transaction.preview_id)?;
    cleanup_transaction_artifacts(&transaction).map_err(|error| ContractFailure {
        exit_code: 7,
        category: "recovery",
        code: "recovery_failed",
        message: "Recovery completed but its protected artifacts could not be cleaned up."
            .to_owned(),
        stage: "recover",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({"transactionId": transaction_id, "failureStage": error}),
    })?;
    store.remove_transaction(transaction_id)?;
    transaction.cleanup_pending = false;
    Ok(json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["recovery", if accept_current { "accept-current" } else { "rollback" }],
        "outcome": outcome,
        "result": {
            "transactionId": transaction_id,
            "recoveryReceiptId": recovery_receipt_id,
            "filesystemState": if accept_current { "changed" } else { "unchanged" },
            "manifestDigest": manifest_digest(&final_manifest),
            "cleanupPending": false
        }
    }))
}

#[derive(Debug)]
struct CommitFailure {
    operation_index: u64,
    _reason: String,
}

fn commit_operations(
    planner: &WorkspaceEditPlanner<'_>,
    operations: &[CanonicalOperation],
) -> Result<(), CommitFailure> {
    for operation in operations {
        let result = match operation {
            CanonicalOperation::Text {
                index,
                path,
                before_digest,
                after_digest,
                edits,
                ..
            } => apply_text_operation(planner, path, before_digest, after_digest, edits).map_err(
                |reason| CommitFailure {
                    operation_index: *index,
                    _reason: reason,
                },
            ),
            CanonicalOperation::Create {
                index,
                path,
                overwrite,
                ignore_if_exists,
                ..
            } => apply_create_operation(planner, path, *overwrite, *ignore_if_exists).map_err(
                |reason| CommitFailure {
                    operation_index: *index,
                    _reason: reason,
                },
            ),
            CanonicalOperation::Rename {
                index,
                old_path,
                new_path,
                overwrite,
                ignore_if_exists,
                ..
            } => apply_rename_operation(planner, old_path, new_path, *overwrite, *ignore_if_exists)
                .map_err(|reason| CommitFailure {
                    operation_index: *index,
                    _reason: reason,
                }),
            CanonicalOperation::Delete {
                index,
                path,
                recursive,
                ignore_if_not_exists,
                ..
            } => apply_delete_operation(planner, path, *recursive, *ignore_if_not_exists).map_err(
                |reason| CommitFailure {
                    operation_index: *index,
                    _reason: reason,
                },
            ),
        };
        result?;
    }
    Ok(())
}

fn apply_text_operation(
    planner: &WorkspaceEditPlanner<'_>,
    path: &Path,
    before_digest: &str,
    after_digest: &str,
    edits: &[super::planner::CanonicalTextEdit],
) -> Result<(), String> {
    let relative = planner.relative_path(path)?;
    let mut read_options = CapabilityOpenOptions::new();
    read_options.read(true).follow(FollowSymlinks::No);
    let mut source = planner
        .capability_root()
        .open_with(relative, &read_options)
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    source
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if digest_raw_bytes(&bytes) != before_digest {
        return Err("Text resource changed during commit.".to_owned());
    }
    let mut result = Vec::with_capacity(bytes.len());
    let mut cursor = 0;
    for edit in edits {
        let start = edit.start_byte as usize;
        let end = edit.end_byte as usize;
        if start < cursor || end < start || end > bytes.len() {
            return Err("Canonical text edit range is invalid.".to_owned());
        }
        result.extend_from_slice(&bytes[cursor..start]);
        result.extend_from_slice(edit.new_text.as_bytes());
        cursor = end;
    }
    result.extend_from_slice(&bytes[cursor..]);
    if digest_raw_bytes(&result) != after_digest {
        return Err("Canonical text edit digest does not match.".to_owned());
    }
    let mut write_options = CapabilityOpenOptions::new();
    write_options.write(true).follow(FollowSymlinks::No);
    let mut file = planner
        .capability_root()
        .open_with(relative, &write_options)
        .map_err(|error| error.to_string())?;
    file.set_len(0).map_err(|error| error.to_string())?;
    file.seek(SeekFrom::Start(0))
        .and_then(|_| file.write_all(&result))
        .and_then(|_| file.sync_all())
        .map_err(|error| error.to_string())
}

fn apply_create_operation(
    planner: &WorkspaceEditPlanner<'_>,
    path: &Path,
    overwrite: bool,
    ignore_if_exists: bool,
) -> Result<(), String> {
    let root = planner.capability_root();
    let relative = planner.relative_path(path)?;
    if root.symlink_metadata(relative).is_ok() {
        if overwrite {
            let mut options = CapabilityOpenOptions::new();
            options
                .write(true)
                .truncate(true)
                .follow(FollowSymlinks::No);
            let file = root
                .open_with(relative, &options)
                .map_err(|error| error.to_string())?;
            return file.sync_all().map_err(|error| error.to_string());
        }
        if ignore_if_exists {
            return Ok(());
        }
        return Err("CreateFile target exists.".to_owned());
    }
    let mut options = CapabilityOpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    let file = root
        .open_with(relative, &options)
        .map_err(|error| error.to_string())?;
    file.sync_all().map_err(|error| error.to_string())
}

fn apply_rename_operation(
    planner: &WorkspaceEditPlanner<'_>,
    old_path: &Path,
    new_path: &Path,
    overwrite: bool,
    ignore_if_exists: bool,
) -> Result<(), String> {
    let root = planner.capability_root();
    let old_relative = planner.relative_path(old_path)?;
    let new_relative = planner.relative_path(new_path)?;
    if root.symlink_metadata(new_relative).is_ok() {
        if overwrite {
            remove_capability_resource(root, new_relative).map_err(|error| error.to_string())?;
        } else if ignore_if_exists {
            return Ok(());
        } else {
            return Err("RenameFile destination exists.".to_owned());
        }
    }
    root.rename(old_relative, root, new_relative)
        .map_err(|error| error.to_string())
}

fn apply_delete_operation(
    planner: &WorkspaceEditPlanner<'_>,
    path: &Path,
    recursive: bool,
    ignore_if_not_exists: bool,
) -> Result<(), String> {
    let root = planner.capability_root();
    let relative = planner.relative_path(path)?;
    let Ok(metadata) = root.symlink_metadata(relative) else {
        return if ignore_if_not_exists {
            Ok(())
        } else {
            Err("DeleteFile target is missing.".to_owned())
        };
    };
    if metadata.is_dir() && !recursive {
        root.remove_dir(relative).map_err(|error| error.to_string())
    } else {
        remove_capability_resource(root, relative).map_err(|error| error.to_string())
    }
}

fn remove_capability_resource(root: &Dir, relative: &Path) -> std::io::Result<()> {
    if root.symlink_metadata(relative)?.is_dir() {
        root.remove_dir_all(relative)
    } else {
        root.remove_file(relative)
    }
}

fn planned_backups(before: &[ManifestEntry], artifact_directory: &Path) -> Vec<BackupEntry> {
    let paths = before
        .iter()
        .filter(|entry| entry.exists)
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>();
    let roots = paths
        .iter()
        .filter(|path| {
            !paths
                .iter()
                .any(|candidate| *candidate != **path && path.starts_with(candidate))
        })
        .collect::<Vec<_>>();
    roots
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            let manifest = before.iter().find(|entry| &entry.path == path).unwrap();
            BackupEntry {
                path: path.clone(),
                backup_path: artifact_directory.join(format!("backup-{index}")),
                existed: true,
                resource_kind: match manifest.resource_kind {
                    ResourceKind::File => "file",
                    ResourceKind::Directory => "directory",
                    ResourceKind::Missing => "missing",
                }
                .to_owned(),
            }
        })
        .collect()
}

fn stage_transaction_backups(
    transaction: &TransactionRecord,
    limits: &MutationSettings,
) -> Result<(), ContractFailure> {
    fs::create_dir(&transaction.artifact_directory).map_err(|error| {
        stage_failure(
            &transaction.transaction_id,
            "The same-volume transaction directory cannot be created.",
            error.raw_os_error(),
        )
    })?;
    state_permissions::restrict_directory(&transaction.artifact_directory).map_err(|error| {
        stage_failure(
            &transaction.transaction_id,
            &error.to_string(),
            error.raw_os_error(),
        )
    })?;
    let mut copied_bytes = 0_u64;
    for backup in &transaction.backups {
        copy_resource(&backup.path, &backup.backup_path, &mut copied_bytes).map_err(|error| {
            stage_failure(
                &transaction.transaction_id,
                "A rollback backup cannot be staged.",
                error.raw_os_error(),
            )
        })?;
        if copied_bytes > limits.max_rollback_bytes {
            return Err(ContractFailure {
                exit_code: 6,
                category: "mutation",
                code: "resource_limit_exceeded",
                message: "Rollback staging exceeds the configured byte limit.".to_owned(),
                stage: "stage",
                delivery: "not_applicable",
                retry: "after_change",
                data: json!({
                    "resource": "rollbackBytes",
                    "limit": limits.max_rollback_bytes,
                    "observed": copied_bytes
                }),
            });
        }
    }
    flush_directory(&transaction.artifact_directory).map_err(|error| {
        stage_failure(
            &transaction.transaction_id,
            "The transaction directory cannot be flushed.",
            error.raw_os_error(),
        )
    })
}

fn rollback_transaction(
    transaction: &TransactionRecord,
    planner: &WorkspaceEditPlanner<'_>,
) -> Result<Vec<ManifestEntry>, String> {
    let mut affected = transaction
        .before_manifest
        .iter()
        .chain(transaction.intended_manifest.iter())
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    affected.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    for path in affected {
        if path.exists() {
            remove_resource(&path).map_err(|error| error.to_string())?;
        }
    }
    let mut backups = transaction.backups.clone();
    backups.sort_by_key(|backup| backup.path.components().count());
    for backup in backups {
        if backup.existed {
            if let Some(parent) = backup.path.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            fs::rename(&backup.backup_path, &backup.path).map_err(|error| error.to_string())?;
        }
    }
    let restored = planner
        .inspect_manifest_paths(&transaction_paths(transaction))
        .map_err(|_| "The restored filesystem cannot be inspected.".to_owned())?;
    let mismatches = rollback_manifest_mismatches(&transaction.before_manifest, &restored);
    if mismatches.is_empty() {
        Ok(restored)
    } else {
        Err("The rollback manifest does not match its preconditions.".to_owned())
    }
}

fn transaction_paths(transaction: &TransactionRecord) -> Vec<PathBuf> {
    transaction
        .before_manifest
        .iter()
        .chain(transaction.intended_manifest.iter())
        .map(|entry| entry.path.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn rollback_manifest_mismatches(
    expected: &[ManifestEntry],
    actual: &[ManifestEntry],
) -> Vec<Value> {
    let mut normalized_expected = expected.to_vec();
    let mut normalized_actual = actual.to_vec();
    for entry in normalized_expected
        .iter_mut()
        .chain(normalized_actual.iter_mut())
    {
        entry.identity_digest = None;
    }
    manifest_mismatches(&normalized_expected, &normalized_actual)
}

fn copy_resource(source: &Path, destination: &Path, copied_bytes: &mut u64) -> std::io::Result<()> {
    let metadata = fs::symlink_metadata(source)?;
    if metadata.is_dir() {
        fs::create_dir(destination)?;
        fs::set_permissions(destination, metadata.permissions())?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_resource(
                &entry.path(),
                &destination.join(entry.file_name()),
                copied_bytes,
            )?;
        }
        flush_directory(destination)?;
    } else {
        fs::copy(source, destination)?;
        *copied_bytes = copied_bytes.saturating_add(metadata.len());
        copy_extended_attributes(source, destination)?;
        File::open(destination)?.sync_all()?;
    }
    Ok(())
}

#[cfg(unix)]
fn copy_extended_attributes(source: &Path, destination: &Path) -> std::io::Result<()> {
    for name in xattr::list(source)? {
        if let Some(value) = xattr::get(source, &name)? {
            xattr::set(destination, &name, &value)?;
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn copy_extended_attributes(_source: &Path, _destination: &Path) -> std::io::Result<()> {
    Ok(())
}

fn remove_resource(path: &Path) -> std::io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

fn cleanup_transaction_artifacts(transaction: &TransactionRecord) -> Result<(), String> {
    if transaction.artifact_directory.exists() {
        fs::remove_dir_all(&transaction.artifact_directory).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn inspect_paths_best_effort(template: &[ManifestEntry]) -> Vec<ManifestEntry> {
    let mut entries = template
        .iter()
        .map(|entry| inspect_path_best_effort(&entry.path))
        .collect::<Vec<_>>();
    entries.sort_by(|left, right| left.path.cmp(&right.path));
    entries
}

fn inspect_path_best_effort(path: &Path) -> ManifestEntry {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return ManifestEntry {
            path: path.to_path_buf(),
            exists: false,
            resource_kind: ResourceKind::Missing,
            identity_digest: None,
            content_digest: None,
            metadata_digest: None,
        };
    };
    let resource_kind = if metadata.is_file() {
        ResourceKind::File
    } else if metadata.is_dir() {
        ResourceKind::Directory
    } else {
        ResourceKind::Missing
    };
    ManifestEntry {
        path: path.to_path_buf(),
        exists: resource_kind != ResourceKind::Missing,
        resource_kind,
        identity_digest: None,
        content_digest: (resource_kind == ResourceKind::File)
            .then(|| fs::read(path).ok().map(|bytes| digest_raw_bytes(&bytes)))
            .flatten(),
        metadata_digest: None,
    }
}

pub(crate) fn manifest_mismatches(
    expected: &[ManifestEntry],
    actual: &[ManifestEntry],
) -> Vec<Value> {
    expected
        .iter()
        .filter_map(|expected| {
            let actual = actual.iter().find(|actual| actual.path == expected.path);
            let actual = actual.cloned().unwrap_or(ManifestEntry {
                path: expected.path.clone(),
                exists: false,
                resource_kind: ResourceKind::Missing,
                identity_digest: None,
                content_digest: None,
                metadata_digest: None,
            });
            let code = if expected.exists != actual.exists {
                Some("resource_existence")
            } else if expected.resource_kind != actual.resource_kind {
                Some("resource_kind")
            } else if expected.identity_digest.is_some()
                && expected.identity_digest != actual.identity_digest
            {
                Some("resource_identity")
            } else if expected.content_digest.is_some()
                && expected.content_digest != actual.content_digest
            {
                Some("resource_content")
            } else if expected.metadata_digest.is_some()
                && expected.metadata_digest != actual.metadata_digest
            {
                Some("resource_metadata")
            } else {
                None
            }?;
            Some(json!({
                "code": code,
                "message": "A filesystem precondition no longer matches.",
                "path": expected.path,
                "expectedDigest": expected.content_digest,
                "actualDigest": actual.content_digest
            }))
        })
        .collect()
}

fn preview_stale_failure(stored: &StoredPreview, reasons: Vec<Value>) -> ContractFailure {
    ContractFailure {
        exit_code: 6,
        category: "mutation",
        code: "preview_stale",
        message: "The Preview authorization or immutable inputs changed.".to_owned(),
        stage: "reserve",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({
            "previewId": stored.preview.preview_id,
            "reasons": reasons,
            "preconditions": stored.preview.preconditions
        }),
    }
}

fn application_success(receipt: &ReceiptRecord, outcome: &str) -> Value {
    json!({
        "schemaVersion": 1,
        "ok": true,
        "command": ["apply"],
        "outcome": outcome,
        "result": {
            "previewId": receipt.preview_id,
            "receiptId": receipt.receipt_id,
            "transactionId": receipt.transaction_id,
            "state": "terminal",
            "outcome": outcome,
            "filesystemState": receipt.filesystem_state,
            "sessionSynchronized": receipt.session_synchronized,
            "cleanupPending": receipt.cleanup_pending,
            "durability": {
                "directoryFlush": receipt.durability["directoryFlush"]
            },
            "manifest": receipt.observed_manifest
        }
    })
}

fn lock_workspace(
    file: &File,
    workspace_uri: &str,
    timeout_text: &str,
) -> Result<(), ContractFailure> {
    let timeout = parse_duration(timeout_text).unwrap_or(Duration::from_secs(30));
    let deadline = Instant::now() + timeout;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(ContractFailure {
                    exit_code: 4,
                    category: "unavailable",
                    code: "workspace_lock_timeout",
                    message: "The Workspace Application lock timed out.".to_owned(),
                    stage: "lock_workspace",
                    delivery: "not_applicable",
                    retry: "safe",
                    data: json!({"workspaceUri": workspace_uri, "timeout": timeout_text}),
                });
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(ContractFailure {
                    exit_code: 4,
                    category: "unavailable",
                    code: "state_unavailable",
                    message: "The Workspace Application lock failed.".to_owned(),
                    stage: "persist",
                    delivery: "not_applicable",
                    retry: "after_change",
                    data: json!({"recordType": "transaction", "path": "", "osCode": error.raw_os_error()}),
                });
            }
        }
    }
}

fn parse_duration(value: &str) -> Option<Duration> {
    if let Some(value) = value.strip_suffix("ms") {
        value.parse().ok().map(Duration::from_millis)
    } else if let Some(value) = value.strip_suffix('s') {
        value.parse().ok().map(Duration::from_secs)
    } else if let Some(value) = value.strip_suffix('m') {
        value
            .parse::<u64>()
            .ok()
            .map(|value| Duration::from_secs(value.saturating_mul(60)))
    } else {
        None
    }
}

fn parse_position_encoding(value: &str) -> PositionEncoding {
    if value == "utf-16" {
        PositionEncoding::Utf16
    } else {
        PositionEncoding::Utf8
    }
}

fn recovery_required_failure(transaction: &TransactionRecord) -> ContractFailure {
    ContractFailure {
        exit_code: 7,
        category: "recovery",
        code: "recovery_required",
        message: "Workspace Recovery is required before another Application can run.".to_owned(),
        stage: "recover",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({
            "transactionId": transaction.transaction_id,
            "receiptId": transaction.receipt_id,
            "manifestDigest": transaction.manifest_digest,
            "intended": transaction.intended_manifest,
            "observed": transaction.observed_manifest
        }),
    }
}

fn transaction_id_from_evidence(evidence: &Value) -> String {
    evidence
        .get("transactionId")
        .and_then(Value::as_str)
        .unwrap_or("txn_00000000000000000000000000000000")
        .to_owned()
}

fn unsupported_filesystem_failure(
    workspace_uri: &str,
    problems: &[WorkspaceEditProblem],
) -> ContractFailure {
    ContractFailure {
        exit_code: 3,
        category: "blocked",
        code: "unsupported_filesystem",
        message: "The Workspace filesystem cannot provide required Mutation safety guarantees."
            .to_owned(),
        stage: "validate_mutation",
        delivery: "not_applicable",
        retry: "after_change",
        data: json!({
            "workspaceUri": workspace_uri,
            "missingCapabilities": problems.iter().map(|problem| &problem.code).collect::<Vec<_>>()
        }),
    }
}

fn stage_failure(transaction_id: &str, message: &str, os_code: Option<i32>) -> ContractFailure {
    let mut data =
        json!({"recordType": "transaction", "path": "", "transactionId": transaction_id});
    if let Some(os_code) = os_code {
        data["osCode"] = json!(os_code);
    }
    ContractFailure {
        exit_code: 4,
        category: "unavailable",
        code: "state_unavailable",
        message: message.to_owned(),
        stage: "persist",
        delivery: "not_applicable",
        retry: "after_change",
        data,
    }
}

fn durability_value() -> Value {
    json!({
        "fileFlush": "complete",
        "directoryFlush": if cfg!(unix) { "complete" } else { "unsupported" }
    })
}

#[cfg(unix)]
fn flush_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn flush_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;

    use super::*;
    use crate::mutation::{PreviewRecordContext, create_preview_record};

    #[test]
    fn exact_text_preview_applies_once_and_returns_same_receipt() {
        let workspace = TempDir::new().unwrap();
        let state = TempDir::new().unwrap();
        let file = workspace.path().join("main.rs");
        fs::write(&file, "old\n").unwrap();
        let store = MutationStateStore::open_at(state.path().join("state")).unwrap();
        let preview_limits = PreviewSettings {
            max_count: 64,
            max_total_bytes: 1_000_000,
            max_document_text_bytes: 1_000_000,
            max_text_bytes: 1_000_000,
        };
        let receipt_limits = ReceiptSettings { max_count: 100 };
        let mutation_limits = MutationSettings {
            application_lock_timeout: "1s".to_owned(),
            max_entries: 100,
            max_recursion_depth: 20,
            max_rollback_bytes: 1_000_000,
            max_staged_text_bytes: 1_000_000,
            max_preauthorized_callbacks: 64,
        };
        let workspace_uri = url::Url::from_directory_path(workspace.path())
            .unwrap()
            .to_string();
        let file_uri = url::Url::from_file_path(&file).unwrap().to_string();
        let planner = WorkspaceEditPlanner::open(
            workspace.path(),
            &workspace_uri,
            PositionEncoding::Utf8,
            &preview_limits,
            &mutation_limits,
        )
        .unwrap();
        let edit = json!({"changes": {file_uri: [{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}}, "newText": "new"}]}});
        let planned = planner.plan_workspace_edit(&edit).unwrap();
        let id = store.new_preview_id().unwrap();
        let record = create_preview_record(
            PreviewRecordContext {
                preview_id: &id,
                workspace_uri: &workspace_uri,
                server: None,
                session_identity: &format!("sid_{}", "0".repeat(64)),
                position_encoding: "utf-8",
                source: json!({"kind": "test"}),
                edit,
                command: None,
            },
            planned,
        );
        store
            .create_preview(
                record,
                workspace.path().to_path_buf(),
                "sha256:test".to_owned(),
                None,
                &preview_limits,
            )
            .unwrap();
        let context = ApplicationContext {
            store: &store,
            preview_limits: &preview_limits,
            receipt_limits: &receipt_limits,
            mutation_limits: &mutation_limits,
            reauthorize: None,
        };
        let first = apply_preview(&context, &id).unwrap();
        let second = apply_preview(&context, &id).unwrap();
        assert_eq!(fs::read_to_string(file).unwrap(), "new\n");
        assert_eq!(first["outcome"], "applied");
        assert_eq!(second["outcome"], "already_applied");
    }
}
