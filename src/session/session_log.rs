use std::collections::VecDeque;

use serde::Serialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

pub(crate) const SESSION_LOG_CAPACITY_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct SessionLogRecord {
    sequence: u64,
    timestamp: String,
    kind: &'static str,
    level: &'static str,
    message: String,
    truncated: bool,
    omitted_bytes: u64,
}

pub(crate) struct SessionLog {
    records: VecDeque<(usize, SessionLogRecord)>,
    retained_bytes: usize,
    evicted_records: u64,
    evicted_bytes: u64,
    next_sequence: u64,
}

impl SessionLog {
    pub(crate) fn new() -> Self {
        Self {
            records: VecDeque::new(),
            retained_bytes: 0,
            evicted_records: 0,
            evicted_bytes: 0,
            next_sequence: 0,
        }
    }

    pub(crate) fn push(
        &mut self,
        kind: &'static str,
        level: &'static str,
        message: impl Into<String>,
    ) {
        let message = message.into();
        let mut record = SessionLogRecord {
            sequence: self.next_sequence,
            timestamp: OffsetDateTime::now_utc()
                .format(&Rfc3339)
                .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_owned()),
            kind,
            level,
            message,
            truncated: false,
            omitted_bytes: 0,
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        let mut size = serialized_size(&record);
        if size > SESSION_LOG_CAPACITY_BYTES {
            let overhead = size.saturating_sub(record.message.len());
            let keep = SESSION_LOG_CAPACITY_BYTES.saturating_sub(overhead + 32);
            let start = utf8_suffix_start(&record.message, keep);
            record.omitted_bytes = start as u64;
            record.message = record.message[start..].to_owned();
            record.truncated = true;
            size = serialized_size(&record);
        }
        while self.retained_bytes.saturating_add(size) > SESSION_LOG_CAPACITY_BYTES {
            let Some((removed_size, _)) = self.records.pop_front() else {
                break;
            };
            self.retained_bytes = self.retained_bytes.saturating_sub(removed_size);
            self.evicted_records = self.evicted_records.saturating_add(1);
            self.evicted_bytes = self.evicted_bytes.saturating_add(removed_size as u64);
        }
        self.retained_bytes = self.retained_bytes.saturating_add(size);
        self.records.push_back((size, record));
    }

    pub(crate) fn render(&self, owner_generation: &str, tail: usize) -> Value {
        let skip = self.records.len().saturating_sub(tail);
        json!({
            "ownerGeneration": owner_generation,
            "capacityBytes": SESSION_LOG_CAPACITY_BYTES,
            "retainedBytes": self.retained_bytes,
            "evictedRecords": self.evicted_records,
            "evictedBytes": self.evicted_bytes,
            "records": self.records.iter().skip(skip).map(|(_, record)| record).collect::<Vec<_>>()
        })
    }

    pub(crate) fn stderr_tail(&self) -> String {
        let mut retained = self
            .records
            .iter()
            .rev()
            .filter(|(_, record)| record.kind == "server_stderr")
            .map(|(_, record)| record.message.as_str())
            .collect::<Vec<_>>();
        retained.reverse();
        let combined = retained.join("");
        let start = utf8_suffix_start(&combined, 8192);
        combined[start..].to_owned()
    }
}

fn serialized_size(record: &SessionLogRecord) -> usize {
    serde_json::to_vec(record)
        .map(|bytes| bytes.len())
        .unwrap_or(SESSION_LOG_CAPACITY_BYTES.saturating_add(1))
}

fn utf8_suffix_start(value: &str, keep: usize) -> usize {
    let mut start = value.len().saturating_sub(keep);
    while !value.is_char_boundary(start) {
        start += 1;
    }
    start
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_log_and_keeps_utf8_tail() {
        let mut log = SessionLog::new();
        log.push("server_stderr", "error", "😀".repeat(400_000));
        let rendered = log.render("gen_00000000000000000000000000000000", 100);
        assert!(rendered["retainedBytes"].as_u64().unwrap() <= 1024 * 1024);
        assert_eq!(rendered["records"][0]["truncated"], true);
        assert!(rendered["records"][0]["message"].as_str().is_some());
    }
}
