//! Append-only operation log: one JSON line per import, update, delete, or
//! gc — success or failure. This is what lets an operator (and the
//! dashboard's Activity tab) answer "did last night's delta actually run,
//! and how big was it?" for updates that land silently in a running server.
//!
//! The log lives *beside* the index (`<index_path>-activity.jsonl`, the same
//! sibling convention as the store) because a full import swaps the index
//! directory away and history must survive that. Writes are single-line
//! `O_APPEND`, atomic enough for concurrent CLI processes without locking.
//! Logging is best-effort: it never fails the operation it describes.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::config::Config;

/// Most events a single read returns, newest first.
pub const MAX_EVENTS_RETURNED: usize = 100;

/// Rotation guard. One line is ~200 bytes, so this trips around 80k events —
/// decades of hourly deltas. When it trips, the newest half is kept.
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ActivityEvent {
    /// RFC 3339, UTC.
    pub ts: String,
    /// "import" | "update" | "delete" | "gc".
    pub op: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Rows imported or upserted.
    #[serde(default)]
    pub rows: u64,
    /// Documents removed by `ruzz delete`.
    #[serde(default)]
    pub deleted: u64,
    /// Delta rows skipped for an empty primary key.
    #[serde(default)]
    pub skipped: u64,
    #[serde(default)]
    pub duration_ms: u64,
    /// Input file names (not full paths — the log may be shared in a report).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
}

impl ActivityEvent {
    pub fn new(op: &str) -> Self {
        Self {
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            op: op.to_string(),
            ok: true,
            error: None,
            rows: 0,
            deleted: 0,
            skipped: 0,
            duration_ms: 0,
            sources: Vec::new(),
        }
    }

    pub fn failed(mut self, error: &anyhow::Error) -> Self {
        self.ok = false;
        self.error = Some(format!("{:#}", error));
        self
    }
}

/// Where this index's activity log lives: `<index_path>-activity.jsonl`,
/// a sibling of the index so it survives the full-import directory swap.
pub fn activity_path(config: &Config) -> PathBuf {
    let index = &config.server.index_path;
    let mut name = index
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("index"))
        .to_os_string();
    name.push("-activity.jsonl");
    match index.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(name),
        _ => Path::new(".").join(name),
    }
}

/// Append one event. Best-effort by design: a search engine that refuses to
/// import because its *log* is unwritable has its priorities backwards.
pub fn log(config: &Config, event: &ActivityEvent) {
    let path = activity_path(config);
    if let Err(e) = append_line(&path, event) {
        eprintln!(
            "  note: could not write activity log {}: {}",
            path.display(),
            e
        );
    }
}

fn append_line(path: &Path, event: &ActivityEvent) -> anyhow::Result<()> {
    let mut line = serde_json::to_vec(event)?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(&line)?;
    rotate_if_oversized(path, &file)?;
    Ok(())
}

/// Keep the log bounded: past MAX_LOG_BYTES, rewrite it with the newest half
/// of its lines via temp + rename. A concurrent appender can lose its event
/// in the swap window — acceptable for a bound that trips once in decades.
fn rotate_if_oversized(path: &Path, file: &std::fs::File) -> anyhow::Result<()> {
    if file.metadata()?.len() <= MAX_LOG_BYTES {
        return Ok(());
    }
    let raw = std::fs::read_to_string(path)?;
    let lines: Vec<&str> = raw.lines().collect();
    let keep = &lines[lines.len() / 2..];
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, keep.join("\n") + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[derive(Debug, Default, serde::Serialize)]
pub struct DayAggregate {
    pub date: String,
    pub rows: u64,
    pub deleted: u64,
    pub events: u64,
    pub errors: u64,
}

pub struct ActivityLog {
    /// Newest first, at most MAX_EVENTS_RETURNED.
    pub events: Vec<ActivityEvent>,
    /// One entry per calendar day that has events, oldest first.
    pub days: Vec<DayAggregate>,
    pub total_events: u64,
}

/// Read the whole log (bounded by rotation), tolerating a torn tail: a line
/// that does not parse is skipped, not fatal — it may be mid-append.
pub fn read_log(path: &Path) -> ActivityLog {
    let raw = std::fs::read_to_string(path).unwrap_or_default();
    let mut events: Vec<ActivityEvent> = raw
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect();
    let total_events = events.len() as u64;

    let mut days: BTreeMap<String, DayAggregate> = BTreeMap::new();
    for event in &events {
        // The date prefix of an RFC 3339 UTC timestamp.
        let date = event.ts.get(0..10).unwrap_or("").to_string();
        if date.is_empty() {
            continue;
        }
        let day = days.entry(date.clone()).or_default();
        day.date = date;
        day.rows += event.rows;
        day.deleted += event.deleted;
        day.events += 1;
        if !event.ok {
            day.errors += 1;
        }
    }

    events.reverse(); // newest first
    events.truncate(MAX_EVENTS_RETURNED);
    ActivityLog {
        events,
        days: days.into_values().collect(),
        total_events,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_log() -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("ruzz-activity-{unique}.jsonl"))
    }

    #[test]
    fn events_roundtrip_and_aggregate_by_day() {
        let path = temp_log();
        let mut a = ActivityEvent::new("import");
        a.ts = "2026-09-01T04:00:00Z".to_string();
        a.rows = 100;
        let mut b = ActivityEvent::new("update");
        b.ts = "2026-09-01T10:00:00Z".to_string();
        b.rows = 7;
        b.skipped = 1;
        let c = ActivityEvent {
            ts: "2026-09-02T04:00:00Z".to_string(),
            ..ActivityEvent::new("update").failed(&anyhow::anyhow!("disk full"))
        };
        for event in [&a, &b, &c] {
            append_line(&path, event).unwrap();
        }
        // A torn tail (partial concurrent append) must not poison the read.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"{\"ts\":\"2026-")
            .unwrap();

        let log = read_log(&path);
        assert_eq!(log.total_events, 3);
        assert_eq!(log.events[0].op, "update");
        assert!(!log.events[0].ok, "newest first");
        assert_eq!(log.events[2].rows, 100);

        assert_eq!(log.days.len(), 2);
        assert_eq!(log.days[0].date, "2026-09-01");
        assert_eq!(log.days[0].rows, 107);
        assert_eq!(log.days[0].events, 2);
        assert_eq!(log.days[0].errors, 0);
        assert_eq!(log.days[1].errors, 1);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_log_reads_empty() {
        let log = read_log(Path::new("/nonexistent/ruzz-activity.jsonl"));
        assert_eq!(log.total_events, 0);
        assert!(log.events.is_empty());
        assert!(log.days.is_empty());
    }
}
