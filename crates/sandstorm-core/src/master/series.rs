//! The fleet over time, for the console.
//!
//! `/metrics` answers "what is it now" and leaves history to whoever scrapes
//! it. Most installs have nobody scraping, so the console would open on a
//! single point. This keeps a small history of its own: every `EVERY`, the
//! same numbers `/metrics` reports are written to sqlite, along with the
//! rates and latencies between two samples and the moments worth a line in
//! a feed -- a node going quiet, a sandbox that will not start.
//!
//! A separate file from the state database, on purpose. The state database
//! is behind one mutex that every request takes; history is written on a
//! timer and read by a page, and neither should queue behind the other.
//!
//! ## Size
//!
//! About forty rows per sample on a six-node fleet, so ~700k rows a day.
//! `KEEP` bounds it: older rows are deleted once an hour. A query asks for a
//! `step` and gets averages per step, so a day-wide chart is a few hundred
//! points whatever the resolution underneath.

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use super::api::{MasterState, MAX_FAILURES};
use super::metrics::HistSnap;
use crate::state::now_secs;
use crate::tier::Tier;

/// How often the fleet is sampled.
pub const EVERY: Duration = Duration::from_secs(5);
/// How much history is kept.
pub const KEEP: Duration = Duration::from_secs(2 * 24 * 3600);

pub struct Series {
    conn: Mutex<Connection>,
}

/// One series: a metric, optionally split by a key (a node, a tier).
#[derive(Debug, Serialize)]
pub struct Line {
    pub metric: String,
    pub key: String,
    /// `[unix seconds, value]`, oldest first.
    pub points: Vec<(i64, f64)>,
}

#[derive(Debug, Serialize)]
pub struct Event {
    pub ts: i64,
    pub kind: String,
    pub subject: String,
    pub detail: String,
}

impl Series {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA synchronous=NORMAL;
             CREATE TABLE IF NOT EXISTS samples (
                 ts     INTEGER NOT NULL,
                 metric TEXT    NOT NULL,
                 key    TEXT    NOT NULL DEFAULT '',
                 value  REAL    NOT NULL
             );
             CREATE INDEX IF NOT EXISTS samples_by_time ON samples(ts);
             CREATE TABLE IF NOT EXISTS events (
                 ts      INTEGER NOT NULL,
                 kind    TEXT    NOT NULL,
                 subject TEXT    NOT NULL,
                 detail  TEXT    NOT NULL DEFAULT ''
             );
             CREATE INDEX IF NOT EXISTS events_by_time ON events(ts);",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn record(&self, ts: i64, rows: &[(&str, String, f64)]) -> Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO samples(ts, metric, key, value) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (metric, key, value) in rows {
                if value.is_finite() {
                    stmt.execute(params![ts, metric, key, value])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn event(&self, ts: i64, kind: &str, subject: &str, detail: &str) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO events(ts, kind, subject, detail) VALUES (?1, ?2, ?3, ?4)",
            params![ts, kind, subject, detail],
        )?;
        Ok(())
    }

    /// Every series since `since`, averaged per `step` seconds.
    pub fn lines(&self, since: i64, step: i64) -> Result<Vec<Line>> {
        let step = step.max(1);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT metric, key, (ts / ?2) * ?2 AS bucket, AVG(value)
             FROM samples WHERE ts >= ?1
             GROUP BY metric, key, bucket ORDER BY metric, key, bucket",
        )?;
        let mut out: BTreeMap<(String, String), Vec<(i64, f64)>> = BTreeMap::new();
        let rows = stmt.query_map(params![since, step], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, f64>(3)?,
            ))
        })?;
        for row in rows {
            let (m, k, t, v) = row?;
            out.entry((m, k)).or_default().push((t, v));
        }
        Ok(out
            .into_iter()
            .map(|((metric, key), points)| Line {
                metric,
                key,
                points,
            })
            .collect())
    }

    pub fn events(&self, since: i64, limit: u32) -> Result<Vec<Event>> {
        Ok(self.events_page(since, &[], "", limit, 0)?.0)
    }

    /// One page of the events since `since`, newest first, and how many
    /// match in all. Only `kinds` if any are given; only those whose subject
    /// or detail contains `q`, ignoring case, if it is not empty.
    pub fn events_page(
        &self,
        since: i64,
        kinds: &[String],
        q: &str,
        limit: u32,
        offset: u32,
    ) -> Result<(Vec<Event>, u64)> {
        use rusqlite::types::Value;
        let mut filter = String::from(" FROM events WHERE ts >= ?");
        let mut args = vec![Value::Integer(since)];
        if !kinds.is_empty() {
            filter += &format!(" AND kind IN ({})", vec!["?"; kinds.len()].join(","));
            args.extend(kinds.iter().map(|k| Value::Text(k.clone())));
        }
        if !q.is_empty() {
            let like = format!(
                "%{}%",
                q.replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_")
            );
            filter += " AND (subject LIKE ? ESCAPE '\\' OR detail LIKE ? ESCAPE '\\')";
            args.push(Value::Text(like.clone()));
            args.push(Value::Text(like));
        }
        let conn = self.conn.lock();
        let total: u64 = conn.query_row(
            &format!("SELECT COUNT(*){filter}"),
            rusqlite::params_from_iter(args.iter()),
            |r| r.get(0),
        )?;
        args.push(Value::Integer(limit.into()));
        args.push(Value::Integer(offset.into()));
        let mut stmt = conn.prepare(&format!(
            "SELECT ts, kind, subject, detail{filter} ORDER BY ts DESC, rowid DESC LIMIT ? OFFSET ?"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(args.iter()), |r| {
            Ok(Event {
                ts: r.get(0)?,
                kind: r.get(1)?,
                subject: r.get(2)?,
                detail: r.get(3)?,
            })
        })?;
        Ok((rows.collect::<rusqlite::Result<_>>()?, total))
    }

    pub fn prune(&self, before: i64) -> Result<usize> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM samples WHERE ts < ?1", params![before])?;
        conn.execute("DELETE FROM events WHERE ts < ?1", params![before])?;
        Ok(n)
    }
}

/// What the sampler remembers between two samples, to turn counters into
/// rates and states into events.
#[derive(Default)]
struct Previous {
    at: Option<std::time::Instant>,
    placed: u64,
    refused: u64,
    deletes: u64,
    woken: u64,
    woken_from: [u64; 4],
    demotions: [u64; 4],
    api: HistSnap,
    api_errors: u64,
    create: HistSnap,
    store_wait: HistSnap,
    up: HashMap<String, bool>,
    failing: HashSet<String>,
}

/// Sample the fleet into `series` forever.
pub async fn sample_loop(st: std::sync::Arc<MasterState>, series: std::sync::Arc<Series>) {
    let mut prev = Previous::default();
    let mut tick = tokio::time::interval(EVERY);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pruned = now_secs();
    loop {
        tick.tick().await;
        let (rows, events) = match take(&st, &mut prev) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "could not sample the fleet for the console");
                continue;
            }
        };
        let series = series.clone();
        let now = now_secs();
        let prune_now = now - pruned > 3600;
        if prune_now {
            pruned = now;
        }
        // sqlite is blocking; keep it off the async workers.
        let res = tokio::task::spawn_blocking(move || -> Result<()> {
            let refs: Vec<(&str, String, f64)> =
                rows.iter().map(|(m, k, v)| (*m, k.clone(), *v)).collect();
            series.record(now, &refs)?;
            for (kind, subject, detail) in events {
                series.event(now, kind, &subject, &detail)?;
            }
            if prune_now {
                series.prune(now - KEEP.as_secs() as i64)?;
            }
            Ok(())
        })
        .await;
        match res {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "could not write the console history"),
            Err(e) => tracing::warn!(error = %e, "console history writer panicked"),
        }
    }
}

type Rows = Vec<(&'static str, String, f64)>;
type Events = Vec<(&'static str, String, String)>;

/// One sample. Sync, so the store lock is never held across an await.
fn take(st: &MasterState, prev: &mut Previous) -> Result<(Rows, Events)> {
    let mut rows: Rows = Vec::with_capacity(64);
    let mut events: Events = Vec::new();

    let (nodes, by_tier, by_node_tier, failed, (snaps, snap_bytes), failing) = {
        let store = st.store();
        (
            store.list_nodes()?,
            store.count_sandboxes_by_tier()?,
            store.count_sandboxes_by_node_tier()?,
            store.count_failed_sandboxes()?,
            store.snapshot_totals()?,
            store.failing_sandboxes(50)?,
        )
    };

    // --- what the fleet holds ---------------------------------------------
    for t in Tier::ORDER {
        let n = by_tier.get(t.as_str()).copied().unwrap_or(0);
        rows.push(("fleet.sandboxes", t.as_str().to_string(), n as f64));
    }
    rows.push(("fleet.failed", String::new(), failed as f64));
    rows.push(("fleet.snapshots", String::new(), snaps as f64));
    rows.push(("fleet.snapshot_bytes", String::new(), snap_bytes as f64));
    let up = nodes.iter().filter(|n| n.connected).count();
    rows.push(("fleet.nodes", "up".into(), up as f64));
    rows.push(("fleet.nodes", "down".into(), (nodes.len() - up) as f64));

    let mut per_node: HashMap<&str, HashMap<&str, u32>> = HashMap::new();
    for (node, tier, n) in &by_node_tier {
        per_node
            .entry(node.as_str())
            .or_default()
            .insert(tier.as_str(), *n);
    }
    for n in &nodes {
        let tiers = per_node.get(n.id.as_str());
        let count = |t: Tier| tiers.and_then(|m| m.get(t.as_str())).copied().unwrap_or(0) as f64;
        let total: f64 = Tier::ORDER.iter().map(|t| count(*t)).sum();
        rows.push(("node.free_mib", n.id.clone(), n.free_mib as f64));
        rows.push(("node.psi", n.id.clone(), n.psi_some_avg10));
        rows.push(("node.sandboxes", n.id.clone(), total));
        rows.push(("node.hot", n.id.clone(), count(Tier::Hot)));
        // The other tiers too, so the console can add a pool's nodes up into
        // the same by-tier lines it draws for the fleet (`node_pools`).
        rows.push(("node.frozen", n.id.clone(), count(Tier::Frozen)));
        rows.push(("node.snapshot", n.id.clone(), count(Tier::Snapshot)));
        rows.push(("node.detached", n.id.clone(), count(Tier::Detached)));
        rows.push(("node.up", n.id.clone(), if n.connected { 1.0 } else { 0.0 }));

        if let Some(was) = prev.up.insert(n.id.clone(), n.connected) {
            if was != n.connected {
                let (kind, detail) = if n.connected {
                    (
                        "node_up",
                        format!("{} is back, {} MiB free", n.id, n.free_mib),
                    )
                } else {
                    (
                        "node_down",
                        format!("{} stopped answering; {} sandboxes on it", n.id, total),
                    )
                };
                events.push((kind, n.id.clone(), detail));
            }
        }
    }

    // --- what happened since the last sample -------------------------------
    let m = &st.metrics;
    let placed = m.creates_placed.load(Relaxed);
    let refused = m.creates_refused.load(Relaxed);
    let deletes = m.deletes.load(Relaxed);
    let woken = m.woken.load(Relaxed);
    let woken_from = m.woken_from_total();
    let demotions = m.demotions_total();
    let (api, api_errors) = m.api_snap();
    let create = m.route_snap("POST", "/v1/sandboxes").unwrap_or_default();
    let store_wait = m.store_wait.snap();

    if let Some(at) = prev.at {
        let secs = at.elapsed().as_secs_f64().max(0.001);
        let rate = |now: u64, then: u64| now.saturating_sub(then) as f64 / secs;
        rows.push(("rate.creates", "placed".into(), rate(placed, prev.placed)));
        rows.push((
            "rate.creates",
            "refused".into(),
            rate(refused, prev.refused),
        ));
        rows.push(("rate.deletes", String::new(), rate(deletes, prev.deletes)));
        rows.push(("rate.wakes", String::new(), rate(woken, prev.woken)));
        // and by the tier each was woken from, keyed like the demotions
        for (i, t) in Tier::ORDER.iter().enumerate() {
            if *t != Tier::Hot {
                rows.push((
                    "rate.wakes",
                    t.as_str().into(),
                    rate(woken_from[i], prev.woken_from[i]),
                ));
            }
        }
        for (i, t) in Tier::ORDER.iter().enumerate() {
            if *t != Tier::Hot {
                rows.push((
                    "rate.demotions",
                    t.as_str().into(),
                    rate(demotions[i], prev.demotions[i]),
                ));
            }
        }
        let api_d = api.since(&prev.api);
        rows.push(("rate.api", "requests".into(), api_d.count as f64 / secs));
        rows.push((
            "rate.api",
            "errors".into(),
            rate(api_errors, prev.api_errors),
        ));
        for (q, name) in [(0.5, "p50"), (0.99, "p99")] {
            if let Some(v) = api_d.quantile(q) {
                rows.push(("latency.api", name.into(), v * 1000.0));
            }
            if let Some(v) = create.since(&prev.create).quantile(q) {
                rows.push(("latency.create", name.into(), v * 1000.0));
            }
        }
        if let Some(v) = store_wait.since(&prev.store_wait).quantile(0.99) {
            rows.push(("latency.store_wait", "p99".into(), v * 1000.0));
        }

        let refused_now = refused.saturating_sub(prev.refused);
        if refused_now > 0 {
            events.push((
                "refused",
                "fleet".into(),
                format!("{refused_now} creates refused: no node had room"),
            ));
        }
        let woke_now = woken.saturating_sub(prev.woken);
        if woke_now > 0 {
            events.push((
                "woken",
                "fleet".into(),
                format!(
                    "{woke_now} {} woken by a request",
                    if woke_now == 1 {
                        "sandbox"
                    } else {
                        "sandboxes"
                    }
                ),
            ));
        }
        let to_disk = demotions[2].saturating_sub(prev.demotions[2]);
        if to_disk > 0 {
            events.push((
                "to_disk",
                "fleet".into(),
                format!("{to_disk} idle sandboxes checkpointed to disk"),
            ));
        }
    }

    let now_failing: HashSet<String> = failing.iter().map(|f| f.0.clone()).collect();
    for (id, node, error, attempts) in &failing {
        if !prev.failing.contains(id) && prev.at.is_some() {
            let kind = if *attempts >= MAX_FAILURES {
                "quarantined"
            } else {
                "failing"
            };
            events.push((kind, id.clone(), format!("on {node}: {error}")));
        }
    }

    *prev = Previous {
        at: Some(std::time::Instant::now()),
        placed,
        refused,
        deletes,
        woken,
        woken_from,
        demotions,
        api,
        api_errors,
        create,
        store_wait,
        up: std::mem::take(&mut prev.up),
        failing: now_failing,
    };
    Ok((rows, events))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_query_averages_each_step_and_keeps_series_apart() {
        let s = Series::open_in_memory().unwrap();
        for (ts, v) in [(100, 1.0), (102, 3.0), (110, 10.0)] {
            s.record(
                ts,
                &[
                    ("node.free_mib", "a".into(), v),
                    ("node.free_mib", "b".into(), v * 2.0),
                ],
            )
            .unwrap();
        }
        let lines = s.lines(0, 10).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].key, "a");
        assert_eq!(lines[0].points, vec![(100, 2.0), (110, 10.0)]);
        assert_eq!(lines[1].points, vec![(100, 4.0), (110, 20.0)]);
    }

    #[test]
    fn pruning_drops_old_rows_and_events_only() {
        let s = Series::open_in_memory().unwrap();
        s.record(10, &[("x", String::new(), 1.0)]).unwrap();
        s.record(20, &[("x", String::new(), 2.0)]).unwrap();
        s.event(10, "node_down", "a", "").unwrap();
        s.event(20, "node_up", "a", "").unwrap();
        assert_eq!(s.prune(15).unwrap(), 1);
        assert_eq!(s.lines(0, 1).unwrap()[0].points, vec![(20, 2.0)]);
        let ev = s.events(0, 10).unwrap();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].kind, "node_up");
    }

    /// The Activity tab's table: filtered, counted and paged on the master.
    #[test]
    fn events_are_filtered_counted_and_paged() {
        let s = Series::open_in_memory().unwrap();
        for i in 0..30 {
            let (kind, subject) = match i % 3 {
                0 => ("woken", format!("sb-{i}")),
                1 => ("to_disk", format!("sb-{i}")),
                _ => ("node_down", format!("f{i}")),
            };
            s.event(100 + i, kind, &subject, &format!("detail {i}"))
                .unwrap();
        }

        let (page, total) = s.events_page(0, &[], "", 10, 0).unwrap();
        assert_eq!(total, 30);
        assert_eq!(page.len(), 10);
        assert_eq!(page[0].ts, 129, "newest first");
        let (page, _) = s.events_page(0, &[], "", 10, 25).unwrap();
        assert_eq!(
            page.iter().map(|e| e.ts).collect::<Vec<_>>(),
            [104, 103, 102, 101, 100]
        );

        let kinds = ["woken".to_string(), "node_down".to_string()];
        let (page, total) = s.events_page(0, &kinds, "", 100, 0).unwrap();
        assert_eq!(total, 20);
        assert!(page.iter().all(|e| e.kind != "to_disk"));

        // Subject or detail, any case; % and _ are matched as themselves.
        assert_eq!(s.events_page(0, &[], "SB-1", 100, 0).unwrap().1, 8);
        assert_eq!(s.events_page(0, &[], "detail 2", 100, 0).unwrap().1, 11);
        assert_eq!(s.events_page(0, &[], "%", 100, 0).unwrap().1, 0);
        assert_eq!(s.events_page(0, &[], "_", 100, 0).unwrap().1, 0);

        assert_eq!(s.events_page(120, &[], "", 100, 0).unwrap().1, 10, "since");
    }

    #[test]
    fn a_value_that_is_not_a_number_is_not_written() {
        let s = Series::open_in_memory().unwrap();
        s.record(
            1,
            &[("x", String::new(), f64::NAN), ("y", String::new(), 1.0)],
        )
        .unwrap();
        let lines = s.lines(0, 1).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].metric, "y");
    }
}
