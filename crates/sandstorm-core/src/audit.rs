//! Append-only audit log for daemon requests.
//!
//! One line of JSON per request. Fields:
//!   {ts: RFC3339, method, path, status, latency_us, remote, ua}
//!
//! Writes go through a single `Mutex<BufWriter<File>>` so concurrent
//! requests serialize on the lock for the line write only; the bulk
//! of request handling stays parallel.
//!
//! Designed to be tailed by an external log shipper (vector, fluentbit).
//! Rotation is external: rename the file and send SIGHUP so the daemon
//! atomically reopens the configured path without dropping records.
use anyhow::{Context, Result};
use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use parking_lot::Mutex;
use serde_json::json;
use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub struct AuditSink {
    inner: Arc<AuditInner>,
}

struct AuditInner {
    writer: Mutex<BufWriter<File>>,
    path: PathBuf,
}

impl AuditSink {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create audit log parent {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("open audit log {}", path.display()))?;
        Ok(Self {
            inner: Arc::new(AuditInner {
                writer: Mutex::new(BufWriter::new(file)),
                path,
            }),
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.inner.path
    }

    /// Flush the current audit file and reopen the configured path.
    ///
    /// The writer lock is held across flush, open, and replacement, so a
    /// concurrent request writes wholly to either the rotated file or the
    /// new file. If opening the new path fails, the old writer remains in
    /// place and callers can retry on the next SIGHUP.
    pub fn reopen(&self) -> Result<()> {
        let mut writer = self.inner.writer.lock();
        writer
            .flush()
            .with_context(|| format!("flush audit log {}", self.inner.path.display()))?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.inner.path)
            .with_context(|| format!("reopen audit log {}", self.inner.path.display()))?;
        let mut replacement = BufWriter::new(file);
        writeln!(
            replacement,
            "{}",
            json!({
                "ts": now_rfc3339(),
                "event": "log_reopened",
            })
        )
        .with_context(|| format!("write reopen event to {}", self.inner.path.display()))?;
        replacement
            .flush()
            .with_context(|| format!("flush reopened audit log {}", self.inner.path.display()))?;
        *writer = replacement;
        Ok(())
    }

    pub fn write(&self, line: serde_json::Value) {
        let mut w = self.inner.writer.lock();
        if let Err(e) = writeln!(w, "{line}") {
            tracing::warn!(error=%e, "audit write failed");
            return;
        }
        if let Err(e) = w.flush() {
            tracing::warn!(error=%e, "audit flush failed");
        }
    }
}

/// axum middleware that emits one audit line per request after the
/// handler returns. Captures method, path, status, wall-clock latency.
pub async fn audit_layer(sink: AuditSink, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let ua = req
        .headers()
        .get("user-agent")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let start = Instant::now();
    let resp = next.run(req).await;
    let latency_us = start.elapsed().as_micros();
    let status = resp.status().as_u16();
    let line = json!({
        "ts": now_rfc3339(),
        "method": method.as_str(),
        "path": path,
        "status": status,
        "latency_us": latency_us as u64,
        "ua": ua,
    });
    sink.write(line);
    resp
}

fn now_rfc3339() -> String {
    // We log audits with seconds precision; sub-second latency goes in
    // the `latency_us` field. `time::OffsetDateTime::now_utc()` reads the
    // system clock and can never silently collapse a pre-epoch clock to
    // 1970 (the previous hand-rolled formatter did, see #158).
    format_unix_seconds_as_rfc3339(time::OffsetDateTime::now_utc().unix_timestamp())
}

fn format_unix_seconds_as_rfc3339(secs: i64) -> String {
    // Use `time` to avoid the y2262 i64→i32 overflow and the silent
    // pre-epoch collapse that the previous hand-rolled formatter had
    // (issue #158). `time` is a small, no-default-features dep already
    // in axum's transitive tree.
    let format =
        time::macros::format_description!("[year]-[month]-[day]T[hour]:[minute]:[second]Z");
    match time::OffsetDateTime::from_unix_timestamp(secs) {
        Ok(dt) => dt
            .format(&format)
            .unwrap_or_else(|_| "0000-00-00T00:00:00Z".to_string()),
        Err(_) => {
            // Out-of-range: timestamp falls outside what `time` can
            // represent (~year ±9999). Emit a sentinel rather than
            // pretending the value was 0. The audit-log consumer can
            // grep for "outside-time-range" to find these.
            "outside-time-range".to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_format_epoch_zero() {
        assert_eq!(format_unix_seconds_as_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn rfc3339_format_known_timestamp() {
        // 2024-01-01T00:00:00Z == 1704067200
        assert_eq!(
            format_unix_seconds_as_rfc3339(1_704_067_200),
            "2024-01-01T00:00:00Z"
        );
    }

    // Regression tests for issue #158.

    #[test]
    fn rfc3339_format_negative_pre_epoch() {
        // Previously the unwrap_or(0) path collapsed pre-epoch clocks to
        // "1970-01-01T00:00:00Z" silently. Now the negative i64 just
        // produces the actual prior date instead of a misleading zero.
        // -1 second == 1969-12-31T23:59:59Z.
        assert_eq!(format_unix_seconds_as_rfc3339(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn rfc3339_format_far_future_does_not_overflow() {
        // Year 9999 fits in time::OffsetDateTime's range.
        // 253_402_300_799 == 9999-12-31T23:59:59Z.
        assert_eq!(
            format_unix_seconds_as_rfc3339(253_402_300_799),
            "9999-12-31T23:59:59Z"
        );
    }

    #[test]
    fn rfc3339_format_out_of_range_returns_sentinel() {
        // Beyond year ±9999 — `time` rejects the construction. We don't
        // silently overflow into a wrong-year string; we emit a sentinel
        // the operator can grep for.
        assert_eq!(
            format_unix_seconds_as_rfc3339(1_000_000_000_000_000_000),
            "outside-time-range"
        );
    }

    #[test]
    fn audit_sink_writes_and_persists() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("audit.log");
        let sink = AuditSink::open(&path).unwrap();
        sink.write(json!({"a": 1}));
        sink.write(json!({"a": 2}));
        let contents = std::fs::read_to_string(&path).unwrap();
        let mut lines = contents.lines();
        assert!(lines.next().unwrap().contains("\"a\":1"));
        assert!(lines.next().unwrap().contains("\"a\":2"));
        assert!(lines.next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn audit_sink_reopens_after_external_rotation() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("audit.log");
        let rotated = td.path().join("audit.log.1");
        let sink = AuditSink::open(&path).unwrap();

        sink.write(json!({"before": true}));
        std::fs::rename(&path, &rotated).unwrap();
        sink.reopen().unwrap();
        sink.write(json!({"after": true}));

        let old_contents = std::fs::read_to_string(rotated).unwrap();
        let new_contents = std::fs::read_to_string(path).unwrap();
        assert!(old_contents.contains("\"before\":true"));
        assert!(!old_contents.contains("\"after\":true"));
        assert!(new_contents.contains("\"event\":\"log_reopened\""));
        assert!(new_contents.contains("\"after\":true"));
        assert!(!new_contents.contains("\"before\":true"));
    }

    #[cfg(unix)]
    #[test]
    fn audit_sink_keeps_old_writer_when_reopen_fails() {
        let td = tempfile::TempDir::new().unwrap();
        let path = td.path().join("audit.log");
        let rotated = td.path().join("audit.log.1");
        let sink = AuditSink::open(&path).unwrap();

        sink.write(json!({"before": true}));
        std::fs::rename(&path, &rotated).unwrap();
        std::fs::create_dir(&path).unwrap();

        assert!(sink.reopen().is_err());
        sink.write(json!({"after_failed_reopen": true}));

        let old_contents = std::fs::read_to_string(rotated).unwrap();
        assert!(old_contents.contains("\"before\":true"));
        assert!(old_contents.contains("\"after_failed_reopen\":true"));
    }
}
