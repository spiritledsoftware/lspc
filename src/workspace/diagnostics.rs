use std::collections::BTreeMap;

use serde_json::{Value, json};

#[derive(Debug, Clone)]
struct CachedDiagnostics {
    uri: String,
    version: Option<i64>,
    diagnostics: Value,
    raw_report: Value,
    result_id: Option<String>,
    received_for_version: Option<i64>,
    closed: bool,
    serialized_bytes: u64,
    last_used: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct DiagnosticResult {
    pub(crate) uri: String,
    pub(crate) diagnostics: Value,
    pub(crate) raw_report: Value,
    /// The effective full report. For an `unchanged` response this is reconstructed
    /// from the cached full report while `raw_report` preserves the wire payload.
    pub(crate) effective_report: Value,
    pub(crate) result_id: Option<String>,
    pub(crate) version: Option<i64>,
    pub(crate) fresh: bool,
    pub(crate) complete: bool,
    pub(crate) closed: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct WorkspaceDiagnosticResult {
    pub(crate) effective_report: Value,
    pub(crate) raw_report: Value,
    pub(crate) fresh: bool,
    pub(crate) complete: bool,
    pub(crate) workspace_complete: bool,
}

/// Bounded LRU cache for published and pull-diagnostic effective snapshots.
#[derive(Debug)]
pub(crate) struct DiagnosticCache {
    snapshots: BTreeMap<String, CachedDiagnostics>,
    max_snapshots: usize,
    max_total_bytes: u64,
    total_bytes: u64,
    clock: u64,
}

impl DiagnosticCache {
    pub(crate) fn new(max_snapshots: u64, max_total_bytes: u64) -> Self {
        Self {
            snapshots: BTreeMap::new(),
            max_snapshots: usize::try_from(max_snapshots).unwrap_or(usize::MAX),
            max_total_bytes,
            total_bytes: 0,
            clock: 0,
        }
    }

    /// Exports the bounded Owner cache for one short-lived CLI Query process.
    pub(crate) fn export_state(&mut self) -> Value {
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        Value::Array(
            self.snapshots
                .iter_mut()
                .map(|(key, snapshot)| {
                    snapshot.last_used = clock;
                    json!({
                        "kind": if key.starts_with("push\0") { "push" } else { "pull" },
                        "uri": snapshot.uri,
                        "version": snapshot.version,
                        "diagnostics": snapshot.diagnostics,
                        "rawReport": snapshot.raw_report,
                        "receivedForVersion": snapshot.received_for_version,
                        "closed": snapshot.closed
                    })
                })
                .collect(),
        )
    }

    /// Imports a trusted Owner cache snapshot into a short-lived Query cache.
    pub(crate) fn import_state(&mut self, state: &Value) {
        let Some(records) = state.as_array() else {
            return;
        };
        for record in records {
            let Some(uri) = record.get("uri").and_then(Value::as_str) else {
                continue;
            };
            if record.get("kind").and_then(Value::as_str) == Some("pull") {
                self.apply_pull_report(
                    uri,
                    record.get("rawReport").cloned().unwrap_or(Value::Null),
                );
            } else {
                let version = record.get("version").and_then(Value::as_i64);
                let current_version = record
                    .get("receivedForVersion")
                    .and_then(Value::as_i64)
                    .or(version);
                self.publish(
                    uri,
                    version,
                    record
                        .get("diagnostics")
                        .cloned()
                        .unwrap_or_else(|| Value::Array(Vec::new())),
                    current_version,
                    current_version.is_some(),
                );
            }
            if record
                .get("closed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                self.mark_closed(uri);
            }
        }
    }

    pub(crate) fn publish(
        &mut self,
        uri: &str,
        version: Option<i64>,
        diagnostics: Value,
        current_version: Option<i64>,
        received_after_boundary: bool,
    ) -> DiagnosticResult {
        let raw_report = json!({
            "uri": uri,
            "version": version,
            "diagnostics": diagnostics
        });
        let cached = self.store(
            push_key(uri),
            CachedDiagnostics {
                uri: uri.to_owned(),
                version,
                diagnostics: diagnostics.clone(),
                raw_report: raw_report.clone(),
                result_id: None,
                received_for_version: if received_after_boundary {
                    current_version
                } else {
                    None
                },
                closed: false,
                serialized_bytes: 0,
                last_used: 0,
            },
        );
        if cached {
            self.published(uri, current_version)
        } else {
            DiagnosticResult {
                uri: uri.to_owned(),
                diagnostics,
                effective_report: raw_report.clone(),
                raw_report,
                result_id: None,
                version,
                fresh: version.is_some() && version == current_version,
                complete: false,
                closed: false,
            }
        }
    }

    pub(crate) fn published(
        &mut self,
        uri: &str,
        current_version: Option<i64>,
    ) -> DiagnosticResult {
        self.clock = self.clock.saturating_add(1);
        if let Some(snapshot) = self.snapshots.get_mut(&push_key(uri)) {
            snapshot.last_used = self.clock;
            let exact_version = snapshot.version.is_some() && snapshot.version == current_version;
            let usable_versionless = snapshot.version.is_none()
                && snapshot.received_for_version.is_some()
                && snapshot.received_for_version == current_version;
            if exact_version || usable_versionless {
                return render(snapshot, exact_version, true);
            }
        }
        DiagnosticResult {
            uri: uri.to_owned(),
            diagnostics: Value::Array(Vec::new()),
            raw_report: Value::Null,
            effective_report: Value::Null,
            result_id: None,
            version: current_version,
            fresh: false,
            complete: false,
            closed: false,
        }
    }

    pub(crate) fn apply_pull_report(&mut self, uri: &str, report: Value) -> DiagnosticResult {
        let kind = report.get("kind").and_then(Value::as_str);
        if kind == Some("unchanged") {
            self.clock = self.clock.saturating_add(1);
            if let Some(snapshot) = self.snapshots.get_mut(&pull_key(uri)) {
                snapshot.last_used = self.clock;
                let result_id = report
                    .get("resultId")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| snapshot.result_id.clone());
                snapshot.result_id.clone_from(&result_id);
                let mut result = render(snapshot, true, true);
                result.raw_report = report.clone();
                result.result_id = result_id;
                if let Some(result_id) = &result.result_id {
                    result.effective_report["resultId"] = Value::String(result_id.clone());
                }
                result.effective_report["uri"] = Value::String(uri.to_owned());
                if let Some(version) = report.get("version") {
                    result.effective_report["version"] = version.clone();
                }
                return result;
            }
            return DiagnosticResult {
                uri: uri.to_owned(),
                diagnostics: Value::Array(Vec::new()),
                raw_report: report,
                effective_report: Value::Null,
                result_id: None,
                version: None,
                fresh: false,
                complete: false,
                closed: false,
            };
        }
        let diagnostics = report
            .get("items")
            .cloned()
            .unwrap_or_else(|| Value::Array(Vec::new()));
        let result_id = report
            .get("resultId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        self.store(
            pull_key(uri),
            CachedDiagnostics {
                uri: uri.to_owned(),
                version: None,
                diagnostics: diagnostics.clone(),
                raw_report: report.clone(),
                result_id: result_id.clone(),
                received_for_version: None,
                closed: false,
                serialized_bytes: 0,
                last_used: 0,
            },
        );
        DiagnosticResult {
            uri: uri.to_owned(),
            diagnostics,
            effective_report: report.clone(),
            raw_report: report,
            result_id,
            version: None,
            fresh: true,
            complete: true,
            closed: false,
        }
    }

    /// Reconstructs unchanged main and related Document diagnostic reports.
    pub(crate) fn apply_document_pull_report(
        &mut self,
        uri: &str,
        report: Value,
    ) -> DiagnosticResult {
        let raw_report = report.clone();
        let related_reports = report
            .get("relatedDocuments")
            .and_then(Value::as_object)
            .cloned();
        let mut result = self.apply_pull_report(uri, report);
        let Some(related_reports) = related_reports else {
            return result;
        };
        let mut effective_related = serde_json::Map::new();
        for (related_uri, related_report) in related_reports {
            let related = self.apply_pull_report(&related_uri, related_report.clone());
            result.fresh &= related.fresh;
            result.complete &= related.complete;
            effective_related.insert(
                related_uri,
                if related.effective_report.is_null() {
                    related_report
                } else {
                    related.effective_report
                },
            );
        }
        if !result.effective_report.is_null() {
            result.effective_report["relatedDocuments"] = Value::Object(effective_related);
        }
        result.raw_report = raw_report;
        result
    }

    /// Reconstructs effective full items in a Workspace diagnostic report while
    /// retaining an exact report containing `unchanged` items as metadata.
    pub(crate) fn apply_workspace_pull_report(
        &mut self,
        report: Value,
    ) -> WorkspaceDiagnosticResult {
        let Some(items) = report.get("items").and_then(Value::as_array) else {
            return WorkspaceDiagnosticResult {
                effective_report: report.clone(),
                raw_report: Value::Null,
                fresh: false,
                complete: false,
                workspace_complete: false,
            };
        };
        let mut effective_items = Vec::with_capacity(items.len());
        let mut fresh = true;
        let mut complete = true;
        let mut had_unchanged = false;
        for item in items {
            let Some(uri) = item.get("uri").and_then(Value::as_str) else {
                effective_items.push(item.clone());
                fresh = false;
                complete = false;
                continue;
            };
            had_unchanged |= item.get("kind").and_then(Value::as_str) == Some("unchanged");
            let current = self.apply_pull_report(uri, item.clone());
            fresh &= current.fresh;
            complete &= current.complete;
            if current.effective_report.is_null() {
                effective_items.push(item.clone());
            } else {
                effective_items.push(current.effective_report);
            }
        }
        let mut effective_report = report.clone();
        effective_report["items"] = Value::Array(effective_items);
        WorkspaceDiagnosticResult {
            effective_report,
            raw_report: if had_unchanged { report } else { Value::Null },
            fresh,
            complete,
            workspace_complete: complete,
        }
    }

    pub(crate) fn mark_closed(&mut self, uri: &str) {
        for key in [push_key(uri), pull_key(uri)] {
            if let Some(snapshot) = self.snapshots.get_mut(&key) {
                snapshot.closed = true;
                snapshot.received_for_version = None;
            }
        }
    }

    pub(crate) fn invalidate_pull_result_ids(&mut self) {
        let pull_keys = self
            .snapshots
            .keys()
            .filter(|key| key.starts_with("pull\0"))
            .cloned()
            .collect::<Vec<_>>();
        for key in pull_keys {
            if let Some(snapshot) = self.snapshots.remove(&key) {
                self.total_bytes = self.total_bytes.saturating_sub(snapshot.serialized_bytes);
            }
        }
    }

    pub(crate) fn pull_result_id(&self, uri: &str) -> Option<&str> {
        self.snapshots
            .get(&pull_key(uri))
            .and_then(|snapshot| snapshot.result_id.as_deref())
    }

    pub(crate) fn pull_result_ids(&self) -> Vec<Value> {
        self.snapshots
            .iter()
            .filter(|(key, _)| key.starts_with("pull\0"))
            .filter_map(|(_, snapshot)| {
                snapshot
                    .result_id
                    .as_ref()
                    .map(|value| json!({"uri": snapshot.uri, "value": value}))
            })
            .collect()
    }

    pub(crate) fn all_known(&mut self) -> Vec<DiagnosticResult> {
        self.clock = self.clock.saturating_add(1);
        let clock = self.clock;
        self.snapshots
            .iter_mut()
            .filter(|(key, _)| key.starts_with("push\0"))
            .map(|(_, snapshot)| {
                snapshot.last_used = clock;
                render(snapshot, false, true)
            })
            .collect()
    }

    fn store(&mut self, key: String, mut snapshot: CachedDiagnostics) -> bool {
        self.clock = self.clock.saturating_add(1);
        let serialized_bytes = serde_json::to_vec(&snapshot.raw_report)
            .map(|bytes| bytes.len() as u64)
            .unwrap_or(u64::MAX);
        if let Some(previous) = self.snapshots.remove(&key) {
            self.total_bytes = self.total_bytes.saturating_sub(previous.serialized_bytes);
        }
        if serialized_bytes > self.max_total_bytes || self.max_snapshots == 0 {
            return false;
        }
        self.total_bytes = self.total_bytes.saturating_add(serialized_bytes);
        snapshot.serialized_bytes = serialized_bytes;
        snapshot.last_used = self.clock;
        self.snapshots.insert(key.clone(), snapshot);
        while self.snapshots.len() > self.max_snapshots || self.total_bytes > self.max_total_bytes {
            let Some(oldest) = self
                .snapshots
                .iter()
                .min_by_key(|(_, snapshot)| snapshot.last_used)
                .map(|(uri, _)| uri.clone())
            else {
                break;
            };
            if let Some(removed) = self.snapshots.remove(&oldest) {
                self.total_bytes = self.total_bytes.saturating_sub(removed.serialized_bytes);
            }
        }
        self.snapshots.contains_key(&key)
    }
}

fn push_key(uri: &str) -> String {
    format!("push\0{uri}")
}

fn pull_key(uri: &str) -> String {
    format!("pull\0{uri}")
}

fn render(snapshot: &CachedDiagnostics, fresh: bool, complete: bool) -> DiagnosticResult {
    DiagnosticResult {
        uri: snapshot.uri.clone(),
        diagnostics: snapshot.diagnostics.clone(),
        raw_report: snapshot.raw_report.clone(),
        effective_report: snapshot.raw_report.clone(),
        result_id: snapshot.result_id.clone(),
        version: snapshot.version,
        fresh,
        complete,
        closed: snapshot.closed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versioned_and_versionless_publications_have_distinct_freshness() {
        let mut cache = DiagnosticCache::new(4, 4096);
        let versioned = cache.publish(
            "file:///a",
            Some(2),
            json!([{"message":"x"}]),
            Some(2),
            true,
        );
        assert!(versioned.fresh);
        assert!(versioned.complete);

        let versionless = cache.publish("file:///a", None, json!([]), Some(2), true);
        assert!(!versionless.fresh);
        assert!(versionless.complete);
        let later_revision = cache.published("file:///a", Some(3));
        assert!(!later_revision.complete);

        cache.publish("file:///b", None, json!([]), Some(1), false);
        let timeout = cache.published("file:///b", Some(1));
        assert!(!timeout.fresh);
        assert!(!timeout.complete);
        assert_eq!(timeout.diagnostics, json!([]));
    }

    #[test]
    fn pull_unchanged_returns_cached_effective_diagnostics() {
        let mut cache = DiagnosticCache::new(4, 4096);
        cache.apply_pull_report(
            "file:///a",
            json!({"kind":"full", "resultId":"one", "items":[{"message":"x"}]}),
        );
        let unchanged =
            cache.apply_pull_report("file:///a", json!({"kind":"unchanged", "resultId":"two"}));
        assert_eq!(unchanged.diagnostics, json!([{"message":"x"}]));
        assert_eq!(unchanged.result_id.as_deref(), Some("two"));
        assert_eq!(unchanged.raw_report["kind"], "unchanged");
    }

    #[test]
    fn document_pull_reconstructs_related_unchanged_reports() {
        let mut cache = DiagnosticCache::new(8, 8192);
        cache.apply_document_pull_report(
            "file:///main",
            json!({
                "kind":"full",
                "resultId":"main-one",
                "items":[],
                "relatedDocuments": {
                    "file:///related": {
                        "kind":"full",
                        "resultId":"related-one",
                        "items":[{"message":"one"}]
                    }
                }
            }),
        );
        let result = cache.apply_document_pull_report(
            "file:///main",
            json!({
                "kind":"unchanged",
                "resultId":"main-two",
                "relatedDocuments": {
                    "file:///related": {"kind":"unchanged", "resultId":"related-two"}
                }
            }),
        );
        assert_eq!(
            result.effective_report["relatedDocuments"]["file:///related"]["items"],
            json!([{"message":"one"}])
        );
        assert_eq!(
            result.effective_report["relatedDocuments"]["file:///related"]["resultId"],
            "related-two"
        );
    }

    #[test]
    fn oversized_snapshot_is_not_cached() {
        let mut cache = DiagnosticCache::new(1, 8);
        cache.apply_pull_report(
            "file:///a",
            json!({"kind":"full", "items":[{"message":"far too large"}]}),
        );
        assert!(cache.all_known().is_empty());
    }
}
